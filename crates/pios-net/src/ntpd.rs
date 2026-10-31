//! `pios ntpd`: SNTP client. Steps the clock on large offsets, slews small
//! ones, and persists the time so the next boot starts close to reality.

use std::net::{ToSocketAddrs, UdpSocket};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pios_core::config::PiosConfig;
use pios_core::{debug, info, paths, warn, Result};

use crate::sntp;

/// Offsets above this are stepped; smaller ones are slewed with adjtime(3).
const STEP_THRESHOLD_NS: i128 = 128_000_000;

pub fn run(cfg: &PiosConfig) -> Result<()> {
    let mut first = true;
    let mut retry = Duration::from_secs(2);
    loop {
        match cfg.ntp.servers.iter().find_map(|s| query(s).map_err(|e| debug!("ntpd", "{s}: {e}")).ok()) {
            Some(offset) => {
                apply(offset)?;
                if first {
                    std::fs::write(paths::TIME_SYNCED, b"")?;
                    info!("ntpd", "clock synchronised (offset {} ms)", offset / 1_000_000);
                    first = false;
                }
                save_clock();
                retry = Duration::from_secs(2);
                std::thread::sleep(Duration::from_secs(cfg.ntp.interval_secs.max(64)));
            }
            None => {
                warn!("ntpd", "no NTP server reachable, retry in {retry:?}");
                std::thread::sleep(retry);
                retry = (retry * 2).min(Duration::from_secs(64));
            }
        }
    }
}

fn now_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}

fn query(server: &str) -> std::io::Result<i128> {
    let addr = (server, 123)
        .to_socket_addrs()?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| std::io::Error::other("no IPv4 address"))?;
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    sock.connect(addr)?;
    let t1 = now_nanos();
    sock.send(&sntp::request(t1))?;
    let mut buf = [0u8; 64];
    let n = sock.recv(&mut buf)?;
    let t4 = now_nanos();
    sntp::offset(&buf[..n], t1, t4).ok_or_else(|| std::io::Error::other("invalid reply"))
}

fn apply(offset_ns: i128) -> Result<()> {
    if offset_ns.abs() > STEP_THRESHOLD_NS {
        let target = now_nanos() as i128 + offset_ns;
        pios_core::sys::set_realtime((target / 1_000_000_000) as i64, (target % 1_000_000_000) as i64)?;
    } else {
        let delta = libc::timeval {
            tv_sec: (offset_ns / 1_000_000_000) as _,
            tv_usec: ((offset_ns % 1_000_000_000) / 1_000) as _,
        };
        // SAFETY: delta is valid; old value not requested.
        unsafe { libc::adjtime(&delta, std::ptr::null_mut()) };
    }
    Ok(())
}

fn save_clock() {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    if let Some(dir) = std::path::Path::new(paths::SAVED_CLOCK).parent() {
        if dir.exists() {
            let _ = std::fs::write(paths::SAVED_CLOCK, secs.to_string());
        }
    }
}
