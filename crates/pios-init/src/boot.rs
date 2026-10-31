use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pios_core::config::{EngineKind, PiosConfig};
use pios_core::service::DEFAULT_PATH;
use pios_core::sys::{self, PowerAction};
use pios_core::{error, info, paths, warn, Error, Result};
use pios_engine_api::EngineContext;
use pios_supervisor::{Exit, Supervisor};

use crate::status::{self, Led};
use crate::{cmdline_value, engine_dependencies, system_services, EngineSelector, DEFAULT_BOOT_DEVICE};

/// Seconds since the epoch at build time; the clock never starts earlier.
const BUILD_EPOCH: Option<&str> = option_env!("PIOS_BUILD_EPOCH");

pub fn run(select_engine: &EngineSelector) -> ! {
    // The kernel starts init with an almost empty environment.
    std::env::set_var("PATH", DEFAULT_PATH);
    if std::process::id() != 1 {
        warn!("init", "not running as PID 1");
    }
    info!("init", "PiOS {} starting", env!("CARGO_PKG_VERSION"));
    match boot(select_engine) {
        Ok(action) => finish(action),
        Err(e) => emergency(e),
    }
}

fn boot(select_engine: &EngineSelector) -> Result<PowerAction> {
    early_mounts()?;
    status::led(Led::Booting);
    // Ctrl-Alt-Del becomes SIGINT to PID 1 instead of an immediate reboot.
    // SAFETY: plain syscall.
    unsafe { libc::reboot(libc::RB_DISABLE_CAD) };

    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let boot_dev = cmdline_value(&cmdline, "pios.boot").unwrap_or(DEFAULT_BOOT_DEVICE);
    let ro = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    if let Err(e) = sys::mount_once(boot_dev, paths::BOOT_DIR, "vfat", ro, "") {
        warn!("init", "boot partition: {e}; using defaults");
    }
    let cfg = PiosConfig::load(paths::CONFIG_FILE.as_ref())?;
    pios_core::log::set_level(&cfg.log_level);
    clock_floor(None);
    if let Err(e) = sys::sethostname(&cfg.hostname) {
        warn!("init", "{e}");
    }

    pios_storage::setup_data(&cfg.storage)?;
    clock_floor(std::fs::read_to_string(paths::SAVED_CLOCK).ok().and_then(|s| s.trim().parse().ok()));
    if let Err(e) = pios_storage::setup_zram(&cfg.storage) {
        warn!("init", "zram: {e}");
    }
    pios_tune::apply(&cfg.tune, &cfg.storage);

    let mut services = system_services(&cfg);
    let mut milestone = None;
    if cfg.engine.kind != EngineKind::None {
        let engine = select_engine(cfg.engine.kind)
            .ok_or_else(|| Error::Config(format!("engine {:?} is not compiled into this image", cfg.engine.kind)))?;
        let ctx = EngineContext {
            config: cfg.engine.clone(),
            data_dir: paths::DATA_DIR.into(),
            run_dir: paths::RUN_DIR.into(),
            boot_dir: paths::BOOT_DIR.into(),
            depends_on: engine_dependencies(&cfg),
        };
        engine.prepare(&ctx)?;
        services.extend(engine.services(&ctx));
        milestone = Some(engine.ready_service());
        info!("init", "engine: {}", engine.name());
    }

    let mut sup = Supervisor::new(services, Some(paths::CTL_SOCKET), cfg.console_service_logs)?;
    if let Some(svc) = milestone {
        sup.on_ready(
            svc,
            Box::new(|| {
                ENGINE_READY.store(true, Ordering::Relaxed);
                // Plain line so humans and test harnesses can spot it.
                pios_core::log::line(pios_core::log::Level::Info, "pios: engine ready");
                status::led(Led::Ready);
                status::save_boot_log("engine ready");
            }),
        );
    }
    // Spawned after Supervisor::new so the thread inherits the blocked signal
    // mask and every signal keeps reaching the supervisor's signalfd.
    std::thread::spawn(snapshot_if_stuck);
    let exit = sup.run();
    info!("init", "{exit:?} requested");
    sup.shutdown();
    Ok(match exit {
        Exit::Reboot => PowerAction::Reboot,
        Exit::Poweroff => PowerAction::Poweroff,
    })
}

static ENGINE_READY: AtomicBool = AtomicBool::new(false);

/// If the engine is not up after a while, save the log anyway so a stuck
/// boot (no network, failing daemon) can be diagnosed from the SD card.
fn snapshot_if_stuck() {
    let mut waited = 0;
    for at in [90, 300] {
        std::thread::sleep(Duration::from_secs(at - waited));
        waited = at;
        if ENGINE_READY.load(Ordering::Relaxed) {
            return;
        }
        warn!("init", "engine not ready after {at} s");
        status::save_boot_log(&format!("engine not ready after {at} s"));
    }
}

fn early_mounts() -> Result<()> {
    let nsd = libc::MS_NOSUID | libc::MS_NODEV;
    let nsde = nsd | libc::MS_NOEXEC;
    sys::mount_once("proc", "/proc", "proc", nsde, "")?;
    sys::mount_once("sysfs", "/sys", "sysfs", nsde, "")?;
    sys::mount_once("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID, "mode=0755")?;
    sys::mount_once("devpts", "/dev/pts", "devpts", libc::MS_NOSUID | libc::MS_NOEXEC, "gid=5,mode=620,ptmxmode=666")?;
    sys::mount_once("shm", "/dev/shm", "tmpfs", nsd, "mode=1777")?;
    sys::mount_once("run", "/run", "tmpfs", nsd, "mode=0755")?;
    sys::mount_once("tmp", "/tmp", "tmpfs", nsd, "mode=1777")?;
    // /var is volatile; persistent parts are bind mounted from /data later.
    sys::mount_once("var", "/var", "tmpfs", nsd, "mode=0755")?;
    sys::mount_once("cgroup2", "/sys/fs/cgroup", "cgroup2", nsde, "nsdelegate")?;
    for d in [paths::RUN_DIR, "/var/log", "/var/lib", "/var/tmp"] {
        std::fs::create_dir_all(d)?;
    }
    let _ = std::os::unix::fs::symlink("/run", "/var/run");
    let _ = std::os::unix::fs::symlink("/proc/self/fd", "/dev/fd");
    Ok(())
}

/// Never let the clock be earlier than the build time or the saved time;
/// TLS (image pulls) breaks badly when the Pi boots into 1970.
fn clock_floor(saved: Option<i64>) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let floor = BUILD_EPOCH.and_then(|s| s.parse::<i64>().ok()).into_iter().chain(saved).max().unwrap_or(0);
    if floor > now {
        if let Err(e) = sys::set_realtime(floor, 0) {
            warn!("init", "{e}");
        }
    }
}

fn finish(action: PowerAction) -> ! {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let _ = std::fs::write(paths::SAVED_CLOCK, secs.to_string());
    let _ = sys::swapoff("/dev/zram0");
    sys::sync();
    for (_, dst) in pios_storage::PERSISTENT.iter().rev() {
        let _ = sys::umount_lazy(dst);
    }
    if let Err(e) = sys::remount_ro(paths::DATA_DIR) {
        warn!("init", "{e}");
    }
    let _ = sys::umount_lazy(paths::DATA_DIR);
    status::save_boot_log(&format!("{action:?}"));
    let _ = sys::umount_lazy(paths::BOOT_DIR);
    sys::sync();
    info!("init", "{action:?}");
    if let Err(e) = sys::reboot(action) {
        error!("init", "{e}");
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Fatal boot error: keep PID 1 alive (a dying init panics the kernel) and
/// reap orphans so the console stays readable, then reboot after a delay.
fn emergency(e: Error) -> ! {
    error!("init", "boot failed: {e}");
    error!("init", "rebooting in 60 s; fix /boot/pios.toml or the SD card");
    status::led(Led::Error);
    status::save_boot_log("boot failed");
    let until = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < until {
        let mut status = 0;
        // SAFETY: status is a valid out-pointer.
        while unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } > 0 {}
        std::thread::sleep(Duration::from_millis(500));
    }
    sys::sync();
    let _ = sys::reboot(PowerAction::Reboot);
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
