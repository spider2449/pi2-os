//! On-device side: write the inactive slot, arm the trial boot, commit.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use sha2::{Digest, Sha256};

use pios_core::error::IoContext;
use pios_core::ipc::{Request, Response};
use pios_core::sys::{self, with_boot_writable, write_boot_file};
use pios_core::{info, paths, Error, Result};

use crate::*;

const CHUNK: usize = 1 << 20;

pub struct ApplyOptions {
    /// Expected sha256 of the image (hex); verified before arming the trial.
    pub sha256: Option<String>,
    /// Ask PID 1 to reboot into the trial right away.
    pub reboot: bool,
}

fn proc_cmdline() -> Result<String> {
    Ok(std::fs::read_to_string("/proc/cmdline").ctx(|| "read /proc/cmdline".into())?)
}

fn boot_file(name: &str) -> Result<String> {
    let p = Path::new(paths::BOOT_DIR).join(name);
    Ok(std::fs::read_to_string(&p).ctx(|| format!("read {}", p.display()))?)
}

fn active_and_target() -> Result<(String, String)> {
    let cmdline = proc_cmdline()?;
    let active = root_of(&cmdline).ok_or_else(|| Error::msg("no root= on the kernel command line"))?.to_string();
    let target = other_root(&active).ok_or_else(|| Error::msg(format!("{active} is not an A/B root slot")))?;
    Ok((active, target))
}

fn partition_bytes(dev: &str) -> Result<u64> {
    let name = Path::new(dev).file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let sectors: u64 = sys::read_attr(format!("/sys/class/block/{name}/size"))?
        .parse()
        .map_err(|_| Error::msg(format!("bad size for {dev}")))?;
    Ok(sectors * 512)
}

pub fn current_is_trial() -> bool {
    proc_cmdline().is_ok_and(|c| is_trial(&c))
}

/// Stream a rootfs image into the inactive slot and arm a trial boot of it.
pub fn apply(input: &mut dyn Read, opts: &ApplyOptions) -> Result<()> {
    let (active, target) = active_and_target()?;
    // An uncommitted trial must finish first; overwriting the other slot now
    // would destroy the rollback target.
    let default = boot_file(CMDLINE).ok().and_then(|c| root_of(&c).map(str::to_string));
    if current_is_trial() && default.as_deref() != Some(active.as_str()) {
        return Err(Error::msg("an uncommitted trial boot is in progress; wait for it to commit or reboot first"));
    }
    let capacity = partition_bytes(&target)?;
    info!("update", "active {active}, writing {target} ({} MiB)", capacity >> 20);

    let mut dev = OpenOptions::new().write(true).open(&target).ctx(|| format!("open {target}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total: u64 = 0;
    loop {
        let n = read_full(input, &mut buf)?;
        if n == 0 {
            break;
        }
        if total == 0 && (n < 4 || &buf[..4] != SQUASHFS_MAGIC) {
            return Err(Error::msg("input is not a squashfs image; slot left untouched"));
        }
        total += n as u64;
        if total > capacity {
            return Err(Error::msg(format!("image larger than {target} ({} MiB)", capacity >> 20)));
        }
        hasher.update(&buf[..n]);
        dev.write_all(&buf[..n]).ctx(|| format!("write {target}"))?;
    }
    if total == 0 {
        return Err(Error::msg("empty input"));
    }
    dev.sync_all().ctx(|| format!("fsync {target}"))?;
    drop(dev);
    let digest = hex(&hasher.finalize());
    if let Some(expected) = &opts.sha256 {
        if !expected.eq_ignore_ascii_case(&digest) {
            return Err(Error::msg(format!("sha256 mismatch: got {digest}, expected {expected}")));
        }
    }
    // Read back what actually reached the card.
    let readback = hash_prefix(&target, total)?;
    if readback != digest {
        return Err(Error::msg(format!("read-back mismatch on {target}: {readback}")));
    }
    info!("update", "{target}: {total} bytes written and verified (sha256 {digest})");

    let cmdline = boot_file(CMDLINE)?;
    let config = boot_file("config.txt")?;
    with_boot_writable(|| {
        write_boot_file(TRY_CMDLINE, &format!("{}\n", trial_cmdline(cmdline.trim(), &target)))?;
        write_boot_file(TRY_CONFIG, &tryboot_config(&config))
    })?;
    info!("update", "trial boot of {target} armed");
    if opts.reboot {
        ctl(&Request::TryReboot)?;
        info!("update", "rebooting into the trial");
    } else {
        info!("update", "run `pios-ctl tryboot` to boot it once");
    }
    Ok(())
}

/// Called by init once the engine is ready: make a trial boot permanent.
pub fn commit_if_trial() -> Result<bool> {
    let cmdline = proc_cmdline()?;
    if !is_trial(&cmdline) {
        return Ok(false);
    }
    let root = root_of(&cmdline).ok_or_else(|| Error::msg("no root="))?.to_string();
    let current = boot_file(CMDLINE)?;
    with_boot_writable(|| write_boot_file(CMDLINE, &format!("{}\n", with_root(current.trim(), &root))))?;
    info!("update", "update committed: {root} is now the default root");
    Ok(true)
}

/// Human readable slot overview.
pub fn status() -> Result<String> {
    let cmdline = proc_cmdline()?;
    let (active, target) = active_and_target()?;
    let default = boot_file(CMDLINE).ok().and_then(|c| root_of(&c).map(str::to_string)).unwrap_or_default();
    let trial = boot_file(TRY_CMDLINE).ok().and_then(|c| root_of(&c).map(str::to_string));
    let slot_ok = |dev: &str| -> &'static str {
        let mut magic = [0u8; 4];
        match File::open(dev).and_then(|mut f| f.read_exact(&mut magic)) {
            Ok(()) if &magic == SQUASHFS_MAGIC => "squashfs",
            Ok(()) => "empty/unknown",
            Err(_) => "unreadable",
        }
    };
    // /proc/cmdline keeps the trial flag for the whole boot; whether the trial
    // was committed shows in cmdline.txt (the permanent default).
    let state = match (is_trial(&cmdline), default == active) {
        (false, _) => "",
        (true, true) => " (trial boot, committed)",
        (true, false) => " (trial boot, not committed yet)",
    };
    Ok(format!(
        "running : {active}{state}\ndefault : {default}\nother   : {target} ({})\ntryboot : {}\n",
        slot_ok(&target),
        trial.map_or("not armed".to_string(), |r| format!("cmdline.try.txt -> {r}")),
    ))
}

fn read_full(input: &mut dyn Read, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match input.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
}

fn hash_prefix(dev: &str, len: u64) -> Result<String> {
    // Drop cached pages so the read-back really comes from the card.
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "1");
    let mut f = File::open(dev).ctx(|| format!("open {dev}"))?.take(len);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = read_full(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn ctl(req: &Request) -> Result<()> {
    let mut s = UnixStream::connect(paths::CTL_SOCKET).map_err(|e| Error::msg(format!("ctl socket: {e}")))?;
    serde_json::to_writer(&mut s, req).map_err(|e| Error::msg(e.to_string()))?;
    s.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    match serde_json::from_str(&line) {
        Ok(Response::Error { message }) => Err(Error::msg(message)),
        Ok(_) => Ok(()),
        Err(e) => Err(Error::msg(format!("bad ctl response: {e}"))),
    }
}
