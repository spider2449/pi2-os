//! On-device side: write the inactive slot, arm the trial boot, commit.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use pios_core::error::IoContext;
use pios_core::ipc::{Request, Response};
use pios_core::sys::{self, with_boot_writable, write_boot_bytes, write_boot_file};
use pios_core::{info, paths, warn, Error, Result};

use crate::*;

const CHUNK: usize = 1 << 20;
/// Upper bound for a kernel or firmware file held in memory.
const MAX_BOOT_FILE: u64 = 32 << 20;

pub struct ApplyOptions {
    /// Expected sha256 of the input stream (hex); verified before arming.
    pub sha256: Option<String>,
    /// Also replace the shared GPU firmware with the bundle's, once the new
    /// slot has committed. Not protected by rollback.
    pub firmware: bool,
    /// Ask PID 1 to reboot into the trial right away.
    pub reboot: bool,
}

fn proc_cmdline() -> Result<String> {
    Ok(std::fs::read_to_string("/proc/cmdline").ctx(|| "read /proc/cmdline".into())?)
}

fn boot_path(name: &str) -> PathBuf {
    Path::new(paths::BOOT_DIR).join(name)
}

fn boot_file(name: &str) -> Result<String> {
    let p = boot_path(name);
    Ok(std::fs::read_to_string(&p).ctx(|| format!("read {}", p.display()))?)
}

/// A slot directory the firmware would accept for `os_prefix`.
fn has_kernel(dir: &str) -> bool {
    KERNEL_FILES.iter().all(|f| boot_path(&format!("{dir}/{f}")).is_file())
}

/// Command line file the firmware uses for slot `dir` (flat layout: root).
fn cmdline_file(dir: Option<&str>) -> String {
    match dir {
        Some(d) if has_kernel(d) && boot_path(&format!("{d}/{CMDLINE}")).is_file() => format!("{d}/{CMDLINE}"),
        _ => CMDLINE.to_string(),
    }
}

/// Root partition a normal (non-trial) boot uses.
fn default_root() -> Option<String> {
    let config = boot_file(CONFIG).ok()?;
    let file = cmdline_file(configured_slot(&config));
    boot_file(&file).ok().and_then(|c| root_of(&c).map(str::to_string))
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

/// Hashes everything read through it, so the sha256 of a whole bundle can be
/// checked while its members are consumed.
struct HashingReader<'a> {
    inner: &'a mut dyn Read,
    hasher: Sha256,
}

impl Read for HashingReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// What an update brings besides the root filesystem.
struct Payload {
    manifest: Manifest,
    kernel: Vec<(String, Vec<u8>)>,
    firmware: Vec<(String, Vec<u8>)>,
}

/// Write an update into the inactive slot and arm a trial boot of it.
///
/// The input is either a bare squashfs image (the slot keeps the running
/// kernel) or an update bundle built by `cargo xtask bundle`: a tar archive
/// of `manifest.txt` first, then kernel/firmware files, then the rootfs.
pub fn apply(input: &mut dyn Read, opts: &ApplyOptions) -> Result<()> {
    let (active, target) = active_and_target()?;
    // An uncommitted trial must finish first; overwriting the other slot now
    // would destroy the rollback target.
    if current_is_trial() && default_root().as_deref() != Some(active.as_str()) {
        return Err(Error::msg("an uncommitted trial boot is in progress; wait for it to commit or reboot first"));
    }
    let target_dir = slot_dir(&target).ok_or_else(|| Error::msg(format!("{target} has no slot directory")))?;
    let active_dir = slot_dir(&active);
    let capacity = partition_bytes(&target)?;
    info!("update", "active {active}, writing {target} ({} MiB)", capacity >> 20);

    let mut hashing = HashingReader { inner: input, hasher: Sha256::new() };
    let mut head = vec![0u8; 512];
    let n = read_full(&mut hashing, &mut head)?;
    head.truncate(n);
    let payload = if head.starts_with(SQUASHFS_MAGIC) {
        let mut stream = Cursor::new(head).chain(&mut hashing);
        let (size, digest) = write_rootfs(&mut stream, &target, capacity, None)?;
        info!("update", "rootfs only; the slot keeps the running kernel");
        rootfs_only_payload(active_dir.as_deref(), size, digest)?
    } else if head.len() == 512 && &head[257..262] == b"ustar" {
        let mut stream = Cursor::new(head).chain(&mut hashing);
        let payload = read_bundle(&mut stream, &target, capacity)?;
        // Drain the archive's end-of-file padding so the stream hash is complete.
        std::io::copy(&mut stream, &mut std::io::sink())?;
        payload
    } else {
        return Err(Error::msg("input is neither a squashfs image nor an update bundle; slot left untouched"));
    };
    let digest = hex(&hashing.hasher.finalize());
    if let Some(expected) = &opts.sha256 {
        if !expected.eq_ignore_ascii_case(&digest) {
            return Err(Error::msg(format!("sha256 mismatch: got {digest}, expected {expected}")));
        }
    }

    install_slot_files(&payload, active_dir.as_deref(), &target_dir, &target, opts.firmware)?;
    let config = boot_file(CONFIG)?;
    with_boot_writable(|| write_boot_file(TRY_CONFIG, &slot_config(&config, Some(&target_dir), true)))?;
    info!("update", "trial boot of {target} ({}) armed", payload.manifest.version);
    if opts.reboot {
        ctl(&Request::TryReboot)?;
        info!("update", "rebooting into the trial");
    } else {
        info!("update", "run `pios-ctl tryboot` to boot it once");
    }
    Ok(())
}

/// Stream a squashfs image onto `target`, then verify it by read-back.
/// Returns its size and sha256.
fn write_rootfs(input: &mut dyn Read, target: &str, capacity: u64, expected: Option<&str>) -> Result<(u64, String)> {
    let mut dev = OpenOptions::new().write(true).open(target).ctx(|| format!("open {target}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total: u64 = 0;
    loop {
        let n = read_full(input, &mut buf)?;
        if n == 0 {
            break;
        }
        if total == 0 && (n < 4 || &buf[..4] != SQUASHFS_MAGIC) {
            return Err(Error::msg("rootfs is not a squashfs image; slot left untouched"));
        }
        total += n as u64;
        if total > capacity {
            return Err(Error::msg(format!("image larger than {target} ({} MiB)", capacity >> 20)));
        }
        hasher.update(&buf[..n]);
        dev.write_all(&buf[..n]).ctx(|| format!("write {target}"))?;
    }
    if total == 0 {
        return Err(Error::msg("empty rootfs"));
    }
    dev.sync_all().ctx(|| format!("fsync {target}"))?;
    drop(dev);
    let digest = hex(&hasher.finalize());
    if let Some(expected) = expected {
        if expected != digest {
            return Err(Error::msg(format!("rootfs sha256 mismatch: got {digest}, manifest says {expected}")));
        }
    }
    // Read back what actually reached the card.
    let readback = hash_prefix(target, total)?;
    if readback != digest {
        return Err(Error::msg(format!("read-back mismatch on {target}: {readback}")));
    }
    info!("update", "{target}: {total} bytes written and verified (sha256 {digest})");
    Ok((total, digest))
}

fn read_bundle(input: &mut dyn Read, target: &str, capacity: u64) -> Result<Payload> {
    let tar_err = |e: std::io::Error| Error::msg(format!("bundle: {e}"));
    let mut archive = tar::Archive::new(input);
    let mut manifest: Option<Manifest> = None;
    let mut payload = Payload { manifest: Manifest::default(), kernel: Vec::new(), firmware: Vec::new() };
    let mut rootfs = false;
    for entry in archive.entries().map_err(tar_err)? {
        let mut entry = entry.map_err(tar_err)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(tar_err)?.into_owned();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        if name == MANIFEST {
            let mut text = String::new();
            entry.by_ref().take(64 << 10).read_to_string(&mut text)?;
            manifest = Some(Manifest::parse(&text).map_err(Error::msg)?);
            continue;
        }
        let m = manifest.as_ref().ok_or_else(|| Error::msg("bundle must start with manifest.txt"))?;
        let Some(kind) = classify(&name) else {
            warn!("update", "bundle: ignoring unknown member {}", path.display());
            continue;
        };
        let expected = m.sha256(&name).ok_or_else(|| Error::msg(format!("{name} is not in the manifest")))?;
        if kind == Member::Rootfs {
            write_rootfs(&mut entry, target, capacity, Some(expected))?;
            rootfs = true;
            continue;
        }
        if entry.size() > MAX_BOOT_FILE {
            return Err(Error::msg(format!("{name} is implausibly large ({} bytes)", entry.size())));
        }
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut data)?;
        let got = hex(&Sha256::digest(&data));
        if got != expected {
            return Err(Error::msg(format!("{name}: sha256 {got}, manifest says {expected}")));
        }
        match kind {
            Member::Kernel => payload.kernel.push((name, data)),
            _ => payload.firmware.push((name, data)),
        }
    }
    payload.manifest = manifest.ok_or_else(|| Error::msg("bundle has no manifest.txt"))?;
    if !rootfs {
        return Err(Error::msg("bundle has no rootfs.squashfs"));
    }
    for f in KERNEL_FILES {
        if !payload.kernel.iter().any(|(n, _)| n == f) {
            return Err(Error::msg(format!("bundle is missing {f}")));
        }
    }
    Ok(payload)
}

/// A bare rootfs keeps the kernel the system is running: copy it from the
/// active slot (or from the flat layout of an older image).
fn rootfs_only_payload(active_dir: Option<&str>, size: u64, digest: String) -> Result<Payload> {
    let from = active_dir.filter(|d| has_kernel(d));
    let mut manifest = Manifest { version: format!("rootfs-only update ({size} bytes)"), files: Vec::new() };
    let mut kernel = Vec::new();
    for f in KERNEL_FILES {
        let rel = from.map_or(f.to_string(), |d| format!("{d}/{f}"));
        let data = std::fs::read(boot_path(&rel)).ctx(|| format!("read /boot/{rel}"))?;
        manifest.files.push((f.to_string(), hex(&Sha256::digest(&data))));
        kernel.push((f.to_string(), data));
    }
    manifest.files.push((BUNDLE_ROOTFS.to_string(), digest));
    Ok(Payload { manifest, kernel, firmware: Vec::new() })
}

/// Fill the target slot directory: kernel files, command lines, manifest and
/// optionally staged firmware. Everything is read back before the trial is armed.
fn install_slot_files(p: &Payload, active_dir: Option<&str>, dir: &str, target: &str, firmware: bool) -> Result<()> {
    let base = boot_file(&cmdline_file(active_dir))?;
    let normal = format!("{}\n", with_root(base.trim(), target));
    let trial = format!("{}\n", trial_cmdline(base.trim(), target));
    if !p.firmware.is_empty() && !firmware {
        info!("update", "bundle firmware not installed (pass --firmware to replace it)");
    }
    let mut written: Vec<(String, String)> = Vec::new();
    with_boot_writable(|| {
        let d = boot_path(dir);
        let staged = d.join(STAGED_FIRMWARE);
        if staged.exists() {
            std::fs::remove_dir_all(&staged).ctx(|| format!("remove {}", staged.display()))?;
        }
        std::fs::create_dir_all(&d).ctx(|| format!("mkdir {}", d.display()))?;
        // The manifest is removed first and written last: a slot directory
        // with a manifest is complete.
        let _ = std::fs::remove_file(d.join(MANIFEST));
        let mut put = |rel: String, data: &[u8]| -> Result<()> {
            write_boot_bytes(&rel, data)?;
            written.push((rel, hex(&Sha256::digest(data))));
            Ok(())
        };
        for (name, data) in &p.kernel {
            put(format!("{dir}/{name}"), data)?;
        }
        put(format!("{dir}/{CMDLINE}"), normal.as_bytes())?;
        // Written twice because whether the firmware applies os_prefix to the
        // `cmdline=` file is not something to depend on.
        put(format!("{dir}/{TRY_CMDLINE}"), trial.as_bytes())?;
        put(TRY_CMDLINE.to_string(), trial.as_bytes())?;
        if firmware && !p.firmware.is_empty() {
            std::fs::create_dir_all(&staged).ctx(|| format!("mkdir {}", staged.display()))?;
            for (name, data) in &p.firmware {
                put(format!("{dir}/{STAGED_FIRMWARE}/{name}"), data)?;
            }
        }
        Ok(())
    })?;
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "1");
    for (rel, sum) in &written {
        let data = std::fs::read(boot_path(rel)).ctx(|| format!("read back /boot/{rel}"))?;
        if hex(&Sha256::digest(&data)) != *sum {
            return Err(Error::msg(format!("read-back mismatch on /boot/{rel}")));
        }
    }
    with_boot_writable(|| write_boot_file(&format!("{dir}/{MANIFEST}"), &p.manifest.render()))?;
    info!("update", "/boot/{dir}: {} files written and verified", written.len());
    Ok(())
}

/// Called by init once the engine is ready: make a trial boot permanent.
pub fn commit_if_trial() -> Result<bool> {
    let cmdline = proc_cmdline()?;
    if !is_trial(&cmdline) {
        return Ok(false);
    }
    let root = root_of(&cmdline).ok_or_else(|| Error::msg("no root="))?.to_string();
    let dir = slot_dir(&root).filter(|d| has_kernel(d));
    let Some(dir) = dir else {
        // Trial armed by an older release (flat layout): only cmdline.txt.
        let current = boot_file(CMDLINE)?;
        with_boot_writable(|| write_boot_file(CMDLINE, &format!("{}\n", with_root(current.trim(), &root))))?;
        info!("update", "update committed: {root} is now the default root");
        return Ok(true);
    };
    let config = boot_file(CONFIG)?;
    let slot_cmdline = boot_file(&format!("{dir}/{CMDLINE}"))?;
    let staged = boot_path(&dir).join(STAGED_FIRMWARE);
    let mut firmware = Vec::new();
    for name in FIRMWARE_FILES {
        if let Ok(data) = std::fs::read(staged.join(name)) {
            firmware.push((name, data));
        }
    }
    with_boot_writable(|| {
        write_boot_file(CONFIG, &slot_config(&config, Some(&dir), false))?;
        // Keeps the flat-layout fallback (no usable slot directory) consistent.
        write_boot_file(CMDLINE, &slot_cmdline)?;
        for (name, data) in &firmware {
            write_boot_bytes(name, data)?;
        }
        if staged.exists() {
            std::fs::remove_dir_all(&staged).ctx(|| format!("remove {}", staged.display()))?;
        }
        Ok(())
    })?;
    info!("update", "update committed: {root} (/boot/{dir}) is now the default");
    if !firmware.is_empty() {
        let names: Vec<&str> = firmware.iter().map(|(n, _)| *n).collect();
        warn!("update", "GPU firmware replaced ({}); takes effect on the next boot", names.join(", "));
    }
    Ok(true)
}

/// Human readable slot overview.
pub fn status() -> Result<String> {
    let cmdline = proc_cmdline()?;
    let (active, _) = active_and_target()?;
    let default = default_root().unwrap_or_default();
    let trial = boot_path(TRY_CONFIG)
        .exists()
        .then(|| boot_file(TRY_CMDLINE).ok().and_then(|c| root_of(&c).map(str::to_string)))
        .flatten();
    // /proc/cmdline keeps the trial flag for the whole boot; whether the trial
    // was committed shows in the default.
    let state = match (is_trial(&cmdline), default == active) {
        (false, _) => "",
        (true, true) => " (trial boot, committed)",
        (true, false) => " (trial boot, not committed yet)",
    };
    let mut s = format!("running : {active}{state}\ndefault : {default}\n");
    for n in SLOTS {
        let dev = active.trim_end_matches(|c: char| c.is_ascii_digit()).to_string() + &n.to_string();
        let dir = format!("p{n}");
        let rootfs = match File::open(&dev).and_then(|mut f| {
            let mut magic = [0u8; 4];
            f.read_exact(&mut magic).map(|()| magic)
        }) {
            Ok(m) if &m == SQUASHFS_MAGIC => "squashfs",
            Ok(_) => "empty/unknown",
            Err(_) => "unreadable",
        };
        let kernel = if has_kernel(&dir) {
            match boot_file(&format!("{dir}/{MANIFEST}")).ok().and_then(|t| Manifest::parse(&t).ok()) {
                Some(m) => m.version,
                None => "incomplete (no manifest)".into(),
            }
        } else {
            "flat layout (kernel in /boot)".into()
        };
        let staged = if boot_path(&dir).join(STAGED_FIRMWARE).exists() { ", firmware staged" } else { "" };
        s.push_str(&format!("{dir}      : {dev} {rootfs}; {kernel}{staged}\n"));
    }
    s.push_str(&format!("tryboot : {}\n", trial.map_or("not armed".to_string(), |r| format!("armed -> {r}"))));
    // Lets `cargo xtask deploy` tell whether this release accepts bundles.
    s.push_str("bundles : supported\n");
    Ok(s)
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
