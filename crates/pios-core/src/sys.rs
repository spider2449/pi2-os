//! Thin, safe wrappers over the Linux syscalls PiOS needs. Only compiled for
//! Linux targets; pure logic lives elsewhere so it can be tested on any host.

use std::ffi::CString;
use std::io;
use std::path::Path;

use crate::error::{IoContext, Result};

fn cstr(s: &str) -> CString {
    CString::new(s).expect("path contains NUL")
}

fn check(ret: libc::c_int) -> io::Result<()> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn mount(source: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: &str) -> Result<()> {
    let (s, t, f, d) = (cstr(source), cstr(target), cstr(fstype), cstr(data));
    let data_ptr = if data.is_empty() { std::ptr::null() } else { d.as_ptr() as *const libc::c_void };
    // SAFETY: all pointers are valid NUL-terminated strings for the call duration.
    let ret = unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), flags, data_ptr) };
    check(ret).ctx(|| format!("mount {fstype} {source} on {target}"))
}

/// Mount unless `target` is already a mount point (EBUSY is tolerated).
pub fn mount_once(source: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: &str) -> Result<()> {
    let _ = std::fs::create_dir_all(target);
    match mount(source, target, fstype, flags, data) {
        Err(crate::Error::Io { source: e, .. }) if e.raw_os_error() == Some(libc::EBUSY) => Ok(()),
        other => other,
    }
}

pub fn bind_mount(source: &str, target: &str) -> Result<()> {
    std::fs::create_dir_all(source).ctx(|| format!("mkdir {source}"))?;
    std::fs::create_dir_all(target).ctx(|| format!("mkdir {target}"))?;
    mount(source, target, "", libc::MS_BIND, "")
}

pub fn umount_lazy(target: &str) -> Result<()> {
    let t = cstr(target);
    // SAFETY: valid C string.
    check(unsafe { libc::umount2(t.as_ptr(), libc::MNT_DETACH) }).ctx(|| format!("umount {target}"))
}

/// Remount read-only before reboot so the filesystem is clean.
pub fn remount_ro(target: &str) -> Result<()> {
    mount("", target, "", libc::MS_REMOUNT | libc::MS_RDONLY, "")
}

pub fn sethostname(name: &str) -> Result<()> {
    // SAFETY: pointer/len pair describes a valid byte slice.
    check(unsafe { libc::sethostname(name.as_ptr() as *const libc::c_char, name.len()) })
        .ctx(|| format!("sethostname {name}"))
}

pub fn sync() {
    // SAFETY: no arguments.
    unsafe { libc::sync() };
}

#[derive(Debug, Clone, Copy)]
pub enum PowerAction {
    Reboot,
    Poweroff,
}

pub fn reboot(action: PowerAction) -> Result<()> {
    let cmd = match action {
        PowerAction::Reboot => libc::RB_AUTOBOOT,
        PowerAction::Poweroff => libc::RB_POWER_OFF,
    };
    // SAFETY: plain syscall.
    check(unsafe { libc::reboot(cmd) }).ctx(|| "reboot".into())
}

pub fn swapon(path: &str, priority: i32) -> Result<()> {
    let p = cstr(path);
    // <sys/swap.h>: SWAP_FLAG_PREFER = 0x8000, priority in the low 15 bits.
    let flags = 0x8000 | (priority & 0x7fff);
    // SAFETY: valid C string.
    check(unsafe { libc::swapon(p.as_ptr(), flags) }).ctx(|| format!("swapon {path}"))
}

pub fn swapoff(path: &str) -> Result<()> {
    let p = cstr(path);
    // SAFETY: valid C string.
    check(unsafe { libc::swapoff(p.as_ptr()) }).ctx(|| format!("swapoff {path}"))
}

pub fn kill(pid: i32, sig: i32) -> io::Result<()> {
    // SAFETY: plain syscall.
    check(unsafe { libc::kill(pid, sig) })
}

/// Write a value into a sysfs/procfs attribute.
pub fn write_attr(path: impl AsRef<Path>, value: &str) -> Result<()> {
    let path = path.as_ref();
    std::fs::write(path, value).ctx(|| format!("write {} <- {value}", path.display()))
}

pub fn read_attr(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    Ok(std::fs::read_to_string(path).ctx(|| format!("read {}", path.display()))?.trim().to_string())
}

/// Seconds since boot (CLOCK_BOOTTIME), monotonic across clock steps.
pub fn uptime_secs() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: ts is a valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    ts.tv_sec as u64
}

/// Step the wall clock.
pub fn set_realtime(secs: i64, nanos: i64) -> Result<()> {
    let ts = libc::timespec { tv_sec: secs as _, tv_nsec: nanos as libc::c_long };
    // SAFETY: ts is valid for reading.
    check(unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) }).ctx(|| "clock_settime".into())
}

/// Total RAM in bytes as reported by sysinfo(2).
pub fn total_ram() -> u64 {
    // SAFETY: zeroed sysinfo is a valid out-parameter.
    let mut si: libc::sysinfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::sysinfo(&mut si) } < 0 {
        return 0;
    }
    si.totalram as u64 * si.mem_unit as u64
}
