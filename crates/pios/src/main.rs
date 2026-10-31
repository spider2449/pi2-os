//! Multi-call entry point. Dispatches on argv[0] (`init`, `pios-ctl`) or on
//! the first argument (`pios netd`, `pios ntpd`, ...). One binary keeps a
//! single copy of the code in the page cache for every PiOS process.

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(target_os = "linux")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let argv0 = args.first().map(|a| a.rsplit('/').next().unwrap_or(a)).unwrap_or("pios");
    let (cmd, rest) = match argv0 {
        "init" => ("init", &args[1..]),
        "pios-ctl" => ("ctl", &args[1..]),
        _ => (args.get(1).map_or("help", String::as_str), args.get(2..).unwrap_or(&[])),
    };
    std::process::exit(match cmd {
        "init" => pios_init::run(&select_engine),
        "ctl" => pios_ctl::main(rest),
        "netd" => daemon("netd", pios_net::netd::run),
        "ntpd" => daemon("ntpd", pios_net::ntpd::run),
        #[cfg(feature = "engine-docker")]
        "engine-stop" => report("engine", pios_engine_docker::api::stop_all_containers()),
        "version" => {
            println!("pios {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            eprintln!("usage: pios <init|netd|ntpd|ctl|engine-stop|version>");
            2
        }
    });
}

#[cfg(target_os = "linux")]
fn select_engine(kind: pios_core::config::EngineKind) -> Option<Box<dyn pios_engine_api::ContainerEngine>> {
    use pios_core::config::EngineKind;
    match kind {
        #[cfg(feature = "engine-docker")]
        EngineKind::Docker => Some(Box::new(pios_engine_docker::DockerEngine)),
        #[cfg(feature = "engine-native")]
        EngineKind::Native => Some(Box::new(pios_engine_native::NativeEngine)),
        _ => None,
    }
}

/// Run a daemon with the boot configuration.
#[cfg(target_os = "linux")]
fn daemon(name: &str, f: fn(&pios_core::config::PiosConfig) -> pios_core::Result<()>) -> i32 {
    let cfg = match pios_core::config::PiosConfig::load(pios_core::paths::CONFIG_FILE.as_ref()) {
        Ok(c) => c,
        Err(e) => return report(name, Err(e)),
    };
    pios_core::log::set_level(&cfg.log_level);
    report(name, f(&cfg))
}

#[cfg(target_os = "linux")]
fn report(name: &str, r: pios_core::Result<()>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            pios_core::error!(name, "{e}");
            1
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pios only runs on Linux; build with --target armv7-unknown-linux-musleabihf");
    std::process::exit(1);
}
