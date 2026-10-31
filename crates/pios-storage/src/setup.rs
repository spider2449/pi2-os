//! Boot-time storage setup on the device.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use pios_core::config::StorageConfig;
use pios_core::error::IoContext;
use pios_core::sys::{self, write_attr};
use pios_core::{info, paths, warn, Error, Result};

use crate::{mbr, swap};

const BLKPG: u32 = 0x1269;
const BLKPG_RESIZE_PARTITION: i32 = 3;
const EXT4_MAGIC: u16 = 0xef53;

/// Directories on the data partition and where they are bind mounted.
pub const PERSISTENT: &[(&str, &str)] =
    &[("docker", "/var/lib/docker"), ("containerd", "/var/lib/containerd"), ("pios", "/var/lib/pios")];

#[repr(C)]
struct BlkpgPartition {
    start: i64,
    length: i64,
    pno: i32,
    devname: [u8; 64],
    volname: [u8; 64],
}

#[repr(C)]
struct BlkpgIoctlArg {
    op: i32,
    flags: i32,
    datalen: i32,
    data: *mut libc::c_void,
}

/// Grow, check and mount the data partition, then bind persistent dirs.
pub fn setup_data(cfg: &StorageConfig) -> Result<()> {
    let dev = cfg.data_device();
    wait_for(&dev, Duration::from_secs(5))?;
    let grown = cfg.grow_data && grow(cfg).unwrap_or_else(|e| {
        warn!("storage", "grow failed: {e}");
        false
    });

    fsck(&dev);
    if let Err(e) = sys::mount(&dev, paths::DATA_DIR, "ext4", libc::MS_NOATIME, &ext4_data(&cfg.data_mount_options)) {
        if has_ext4(&dev) {
            return Err(e);
        }
        info!("storage", "no filesystem on {dev}, creating ext4");
        run("mke2fs", &["-q", "-t", "ext4", "-L", "pios-data", "-E", "lazy_itable_init=1", &dev])?;
        sys::mount(&dev, paths::DATA_DIR, "ext4", libc::MS_NOATIME, &ext4_data(&cfg.data_mount_options))?;
    }
    if grown {
        // ext4 supports online growth; runs in the background of a mounted fs.
        if let Err(e) = run("resize2fs", &[&dev]) {
            warn!("storage", "resize2fs: {e}");
        }
    }
    for (src, dst) in PERSISTENT {
        sys::bind_mount(&format!("{}/{src}", paths::DATA_DIR), dst)?;
    }
    Ok(())
}

/// Mount options other than the ones expressed as flags.
fn ext4_data(opts: &str) -> String {
    opts.split(',').filter(|o| !o.is_empty() && *o != "noatime").collect::<Vec<_>>().join(",")
}

fn wait_for(dev: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !Path::new(dev).exists() {
        if Instant::now() > deadline {
            return Err(Error::msg(format!("{dev} did not appear")));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn grow(cfg: &StorageConfig) -> Result<bool> {
    let disk_name = Path::new(&cfg.disk).file_name().and_then(|n| n.to_str()).unwrap_or("mmcblk0");
    let disk_sectors: u64 = sys::read_attr(format!("/sys/block/{disk_name}/size"))?
        .parse()
        .map_err(|_| Error::msg("bad disk size"))?;
    let mut f = OpenOptions::new().read(true).write(true).open(&cfg.disk).ctx(|| format!("open {}", cfg.disk))?;
    let mut table = [0u8; 512];
    f.read_exact(&mut table)?;
    let partno = cfg.data_partition as usize;
    let Some(p) = mbr::grow_to_end(&mut table, partno, disk_sectors) else {
        return Ok(false);
    };
    info!("storage", "growing partition {partno} to {} MiB", (p.sectors as u64 * mbr::SECTOR) >> 20);
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&table)?;
    f.sync_all()?;

    // Tell the kernel about the new size without re-reading the whole table
    // (BLKRRPART would fail: the boot and root partitions are in use).
    let mut part = BlkpgPartition {
        start: (p.start as u64 * mbr::SECTOR) as i64,
        length: (p.sectors as u64 * mbr::SECTOR) as i64,
        pno: partno as i32,
        devname: [0; 64],
        volname: [0; 64],
    };
    let mut arg = BlkpgIoctlArg {
        op: BLKPG_RESIZE_PARTITION,
        flags: 0,
        datalen: std::mem::size_of::<BlkpgPartition>() as i32,
        data: &mut part as *mut _ as *mut libc::c_void,
    };
    // SAFETY: arg/part are valid, correctly laid out for BLKPG.
    if unsafe { libc::ioctl(f.as_raw_fd(), BLKPG as _, &mut arg) } < 0 {
        return Err(std::io::Error::last_os_error()).ctx(|| "BLKPG resize".into());
    }
    Ok(true)
}

fn has_ext4(dev: &str) -> bool {
    let Ok(f) = std::fs::File::open(dev) else { return false };
    let mut magic = [0u8; 2];
    f.read_exact_at(&mut magic, 1024 + 56).is_ok() && u16::from_le_bytes(magic) == EXT4_MAGIC
}

fn fsck(dev: &str) {
    if !has_ext4(dev) {
        return;
    }
    match Command::new("e2fsck").args(["-p", dev]).status() {
        Ok(s) if s.code().is_some_and(|c| c <= 1) => {}
        Ok(s) => warn!("storage", "e2fsck {dev} returned {s}"),
        Err(e) => warn!("storage", "e2fsck unavailable: {e}"),
    }
}

fn run(prog: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(prog).args(args).status().ctx(|| format!("exec {prog}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::msg(format!("{prog} exited with {status}")))
    }
}

/// Compressed swap in RAM. On a 1 GB board this absorbs memory spikes from
/// image pulls without touching the SD card.
pub fn setup_zram(cfg: &StorageConfig) -> Result<()> {
    if cfg.zram_fraction <= 0.0 {
        return Ok(());
    }
    let size = ((sys::total_ram() as f64 * cfg.zram_fraction) as u64) & !(swap::PAGE as u64 - 1);
    if let Err(e) = write_attr("/sys/block/zram0/comp_algorithm", &cfg.zram_algorithm) {
        warn!("storage", "zram algorithm {}: {e}", cfg.zram_algorithm);
    }
    write_attr("/sys/block/zram0/disksize", &size.to_string())?;
    let uuid = {
        let mut u = [0u8; 16];
        if let Ok(mut r) = std::fs::File::open("/dev/urandom") {
            let _ = r.read_exact(&mut u);
        }
        u
    };
    let mut dev = OpenOptions::new().write(true).open("/dev/zram0").ctx(|| "open /dev/zram0".into())?;
    dev.write_all(&swap::header(size, uuid))?;
    dev.sync_all()?;
    drop(dev);
    sys::swapon("/dev/zram0", 100)?;
    info!("storage", "zram swap {} MiB ({})", size >> 20, cfg.zram_algorithm);
    Ok(())
}
