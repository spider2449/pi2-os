//! Running container processes, without runc, containerd or a shim.
//!
//! The daemon starts `pios container-init <spec>` (or `container-exec`) as a
//! plain child, already placed in the container's cgroup. That helper is a
//! fresh, single-threaded process, so it can freely unshare namespaces,
//! mount the overlay root and fork; the forked child becomes PID 1 of the new
//! PID namespace, finishes the mount setup, pivots into the root, drops
//! privileges and execs the container command. The helper stays behind as
//! its parent, reports the child's PID to the daemon over fd 3, and exits with
//! the child's status, which is what the daemon waits for.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use pios_core::{Error, Result};

use crate::netlink;
use crate::state::{DeviceMapping, NetMode, ProcessConfig};

/// What `container-init` needs, written by the daemon as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSpec {
    pub layers_dir: String,
    /// Layer directory names relative to `layers_dir`, uppermost first.
    pub lower: Vec<String>,
    pub upper: String,
    pub work: String,
    pub rootfs: String,
    pub readonly_rootfs: bool,
    pub hostname: String,
    pub process: ProcessConfig,
    pub tty: bool,
    pub net: NetMode,
    pub ip: Option<Ipv4Addr>,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
    pub mac: String,
    pub mounts: Vec<SpecMount>,
    pub devices: Vec<DeviceMapping>,
    pub privileged: bool,
    pub caps: Vec<u32>,
    /// Apply the default seccomp filter.
    #[serde(default)]
    pub seccomp: bool,
    pub shm_size: u64,
    pub sysctls: Vec<(String, String)>,
    pub rlimits: Vec<(String, u64, u64)>,
    /// Host path of an init (tini) to run the command under (`--init`).
    pub init: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SpecMount {
    Bind { source: String, target: String, read_only: bool },
    Tmpfs { options: String, target: String },
}

/// What `container-exec` needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecSpec {
    /// Host PID of the container's init.
    pub pid: i32,
    pub process: ProcessConfig,
    pub tty: bool,
    pub caps: Vec<u32>,
    /// Apply the default seccomp filter.
    #[serde(default)]
    pub seccomp: bool,
    pub rlimits: Vec<(String, u64, u64)>,
}

// ---------------------------------------------------------------------------
// Helper side (runs in the fresh `pios container-init|container-exec` process)
// ---------------------------------------------------------------------------

fn errno<T>(what: impl std::fmt::Display) -> std::result::Result<T, String> {
    Err(format!("{what}: {}", io::Error::last_os_error()))
}

fn c(s: impl AsRef<Path>) -> CString {
    CString::new(s.as_ref().as_os_str().as_bytes()).unwrap_or_default()
}

fn mount(src: &str, target: &Path, fstype: &str, flags: libc::c_ulong, data: &str) -> std::result::Result<(), String> {
    let (s, t, f, d) = (c(src), c(target), c(fstype), CString::new(data).unwrap_or_default());
    let dp = if data.is_empty() { std::ptr::null() } else { d.as_ptr().cast() };
    let fp = if fstype.is_empty() { std::ptr::null() } else { f.as_ptr() };
    // SAFETY: valid C strings for the duration of the call.
    if unsafe { libc::mount(s.as_ptr(), t.as_ptr(), fp, flags, dp) } < 0 {
        return errno(format!("mount {fstype} {src} on {}", target.display()));
    }
    Ok(())
}

/// Join `path` under `root`, resolving symlinks inside the root so a link
/// in the image (e.g. /data -> /etc) cannot redirect a mount to the host.
pub fn secure_join(root: &Path, path: &str) -> PathBuf {
    let mut parts: Vec<String> = Vec::new();
    let mut pending: Vec<String> = path.split('/').rev().filter(|s| !s.is_empty()).map(String::from).collect();
    let mut links = 0;
    while let Some(p) = pending.pop() {
        match p.as_str() {
            "." => continue,
            ".." => {
                parts.pop();
                continue;
            }
            _ => {}
        }
        let candidate: PathBuf = root.join(parts.iter().chain(std::iter::once(&p)).collect::<PathBuf>());
        match fs::read_link(&candidate) {
            Ok(target) if links < 40 => {
                links += 1;
                let t = target.to_string_lossy().into_owned();
                if t.starts_with('/') {
                    parts.clear();
                }
                pending.extend(t.split('/').rev().filter(|s| !s.is_empty()).map(String::from));
            }
            _ => parts.push(p),
        }
    }
    root.join(parts.iter().collect::<PathBuf>())
}

/// Create `p` as a directory or an empty file (mount point for `like`).
fn make_target(p: &Path, dir: bool) -> std::result::Result<(), String> {
    if dir {
        fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))
    } else {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        if !p.exists() {
            File::create(p).map_err(|e| format!("create {}: {e}", p.display()))?;
        }
        Ok(())
    }
}

fn bind(source: &Path, target: &Path, read_only: bool) -> std::result::Result<(), String> {
    let src = source.to_string_lossy();
    mount(&src, target, "", libc::MS_BIND | libc::MS_REC, "")?;
    if read_only {
        let flags = libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_REC | libc::MS_NOSUID | libc::MS_NODEV;
        mount("", target, "", flags, "")?;
    }
    Ok(())
}

const MASKED: [&str; 11] = [
    "/proc/asound", "/proc/acpi", "/proc/kcore", "/proc/keys", "/proc/latency_stats", "/proc/timer_list",
    "/proc/timer_stats", "/proc/sched_debug", "/proc/scsi", "/sys/firmware", "/sys/devices/virtual/powercap",
];
const READONLY: [&str; 5] = ["/proc/bus", "/proc/fs", "/proc/irq", "/proc/sys", "/proc/sysrq-trigger"];

/// Mount setup inside the new mount + PID namespace, then pivot_root.
fn setup_root(spec: &RuntimeSpec) -> std::result::Result<(), String> {
    let root = Path::new(&spec.rootfs);
    let nsde = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    let ro_flag = if spec.privileged { 0 } else { libc::MS_RDONLY };

    let proc_ = root.join("proc");
    make_target(&proc_, true)?;
    mount("proc", &proc_, "proc", nsde, "")?;

    let dev = root.join("dev");
    make_target(&dev, true)?;
    if spec.privileged {
        bind(Path::new("/dev"), &dev, false)?;
    } else {
        mount("tmpfs", &dev, "tmpfs", libc::MS_NOSUID | libc::MS_STRICTATIME, "mode=755,size=65536k")?;
        for (name, major, minor) in [("null", 1, 3), ("zero", 1, 5), ("full", 1, 7), ("random", 1, 8), ("urandom", 1, 9), ("tty", 5, 0)] {
            let p = c(dev.join(name));
            // SAFETY: valid path; makedev builds the device number.
            if unsafe { libc::mknod(p.as_ptr(), libc::S_IFCHR | 0o666, libc::makedev(major, minor)) } < 0 {
                return errno(format!("mknod /dev/{name}"));
            }
            // mknod honours the umask; devices must be world accessible.
            let _ = fs::set_permissions(dev.join(name), fs::Permissions::from_mode(0o666));
        }
        let pts = dev.join("pts");
        make_target(&pts, true)?;
        mount("devpts", &pts, "devpts", libc::MS_NOSUID | libc::MS_NOEXEC, "newinstance,ptmxmode=0666,mode=0620,gid=5")?;
        let _ = std::os::unix::fs::symlink("pts/ptmx", dev.join("ptmx"));
        let shm = dev.join("shm");
        make_target(&shm, true)?;
        let size = if spec.shm_size > 0 { spec.shm_size } else { 64 << 20 };
        mount("shm", &shm, "tmpfs", nsde, &format!("mode=1777,size={size}"))?;
        let mq = dev.join("mqueue");
        make_target(&mq, true)?;
        let _ = mount("mqueue", &mq, "mqueue", nsde, "");
        for (link, target) in [("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")] {
            let _ = std::os::unix::fs::symlink(target, dev.join(link));
        }
        if spec.tty {
            // The pty slave is our stdin; expose it as /dev/console too.
            if let Ok(tty) = fs::read_link("/proc/self/fd/0") {
                let console = dev.join("console");
                make_target(&console, false)?;
                bind(&tty, &console, false)?;
            }
        }
        for d in &spec.devices {
            let target = secure_join(root, &d.path_in_container);
            make_target(&target, false)?;
            bind(Path::new(&d.path_on_host), &target, false)?;
        }
    }

    let sys = root.join("sys");
    make_target(&sys, true)?;
    mount("sysfs", &sys, "sysfs", nsde | ro_flag, "")?;
    let cg = sys.join("fs/cgroup");
    if cg.is_dir() {
        mount("cgroup2", &cg, "cgroup2", nsde | ro_flag, "")?;
    }

    for m in &spec.mounts {
        match m {
            SpecMount::Bind { source, target, read_only } => {
                let t = secure_join(root, target);
                let is_dir = Path::new(source).is_dir();
                make_target(&t, is_dir)?;
                bind(Path::new(source), &t, *read_only)?;
            }
            SpecMount::Tmpfs { options, target } => {
                let t = secure_join(root, target);
                make_target(&t, true)?;
                let opts = if options.is_empty() { "mode=1777".to_string() } else { options.clone() };
                mount("tmpfs", &t, "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, &opts)?;
            }
        }
    }
    if let Some(init) = &spec.init {
        let t = root.join("sbin/docker-init");
        make_target(&t, false)?;
        bind(Path::new(init), &t, true)?;
    }
    for (k, v) in &spec.sysctls {
        let p = proc_.join("sys").join(k.replace('.', "/"));
        fs::write(&p, v).map_err(|e| format!("sysctl {k}={v}: {e}"))?;
    }
    if !spec.privileged {
        for m in MASKED {
            let p = root.join(&m[1..]);
            match fs::symlink_metadata(&p) {
                Ok(md) if md.is_dir() => mount("tmpfs", &p, "tmpfs", libc::MS_RDONLY, "")?,
                Ok(_) => bind(&dev.join("null"), &p, false)?,
                Err(_) => {}
            }
        }
        for r in READONLY {
            let p = root.join(&r[1..]);
            if p.exists() {
                bind(&p, &p, true)?;
            }
        }
    }

    // pivot_root(".", ".") stacks the old root under the new one; detach it.
    let r = c(root);
    // SAFETY: valid C strings; standard pivot_root idiom.
    unsafe {
        if libc::chdir(r.as_ptr()) < 0 {
            return errno("chdir rootfs");
        }
        if libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) < 0 {
            return errno("pivot_root");
        }
        if libc::umount2(c".".as_ptr(), libc::MNT_DETACH) < 0 {
            return errno("detach old root");
        }
        if libc::chdir(c"/".as_ptr()) < 0 {
            return errno("chdir /");
        }
    }
    if spec.readonly_rootfs {
        mount("", Path::new("/"), "", libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY, "")?;
    }
    Ok(())
}

/// Look `name` up in a passwd/group style file: (id, gid-or-members).
fn lookup(file: &str, name: &str) -> Option<Vec<String>> {
    let text = fs::read_to_string(file).ok()?;
    text.lines().map(|l| l.split(':').map(String::from).collect::<Vec<_>>()).find(|f| f.first().map(String::as_str) == Some(name))
}

/// Resolve `user[:group]` (names or numbers) inside the container root.
pub fn resolve_user(spec: &str) -> std::result::Result<(u32, u32, Vec<u32>), String> {
    let (u, g) = spec.split_once(':').map_or((spec, None), |(u, g)| (u, Some(g)));
    let (uid, mut gid, name) = if u.is_empty() {
        (0, 0, Some("root".to_string()))
    } else if let Ok(n) = u.parse::<u32>() {
        let entry = fs::read_to_string("/etc/passwd")
            .ok()
            .and_then(|t| t.lines().map(|l| l.split(':').map(String::from).collect::<Vec<_>>()).find(|f| f.get(2).map(String::as_str) == Some(u)));
        let gid = entry.as_ref().and_then(|f| f.get(3)?.parse().ok()).unwrap_or(0);
        (n, gid, entry.and_then(|f| f.first().cloned()))
    } else {
        let f = lookup("/etc/passwd", u).ok_or_else(|| format!("unable to find user {u}: no matching entries in passwd file"))?;
        let uid = f.get(2).and_then(|s| s.parse().ok()).ok_or_else(|| format!("bad passwd entry for {u}"))?;
        let gid = f.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
        (uid, gid, Some(u.to_string()))
    };
    if let Some(g) = g {
        gid = match g.parse::<u32>() {
            Ok(n) => n,
            Err(_) => lookup("/etc/group", g)
                .and_then(|f| f.get(2)?.parse().ok())
                .ok_or_else(|| format!("unable to find group {g}: no matching entries in group file"))?,
        };
    }
    let mut groups = vec![gid];
    if let (Some(name), Ok(text)) = (name, fs::read_to_string("/etc/group")) {
        for l in text.lines() {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() >= 4 && f[3].split(',').any(|m| m == name) {
                if let Ok(n) = f[2].parse::<u32>() {
                    if !groups.contains(&n) {
                        groups.push(n);
                    }
                }
            }
        }
    }
    Ok((uid, gid, groups))
}

fn last_cap() -> u32 {
    fs::read_to_string("/proc/sys/kernel/cap_last_cap").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(40)
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn rlimit_resource(name: &str) -> Option<libc::c_int> {
    Some(match name {
        "nofile" => libc::RLIMIT_NOFILE as libc::c_int,
        "nproc" => libc::RLIMIT_NPROC as libc::c_int,
        "core" => libc::RLIMIT_CORE as libc::c_int,
        "memlock" => libc::RLIMIT_MEMLOCK as libc::c_int,
        "stack" => libc::RLIMIT_STACK as libc::c_int,
        "fsize" => libc::RLIMIT_FSIZE as libc::c_int,
        "nice" => libc::RLIMIT_NICE as libc::c_int,
        "rtprio" => libc::RLIMIT_RTPRIO as libc::c_int,
        "msgqueue" => libc::RLIMIT_MSGQUEUE as libc::c_int,
        "sigpending" => libc::RLIMIT_SIGPENDING as libc::c_int,
        _ => return None,
    })
}

/// Credentials, capabilities, terminal, cwd, then exec. Never returns on
/// success. Runs in the forked child, inside the container root.
fn exec_process(p: &ProcessConfig, tty: bool, caps: &[u32], seccomp: bool, rlimits: &[(String, u64, u64)], init: bool) -> String {
    for (name, soft, hard) in rlimits {
        if let Some(res) = rlimit_resource(name) {
            let rl = libc::rlimit { rlim_cur: *soft as _, rlim_max: *hard as _ };
            // SAFETY: valid rlimit struct.
            if unsafe { libc::setrlimit(res as _, &rl) } < 0 {
                return format!("setrlimit {name}: {}", io::Error::last_os_error());
            }
        }
    }
    let (uid, gid, groups) = match resolve_user(&p.user) {
        Ok(v) => v,
        Err(e) => return e,
    };
    // Bounding set first, while CAP_SETPCAP is still held.
    for cap in 0..=last_cap() {
        if !caps.contains(&cap) {
            // SAFETY: plain prctl.
            unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) };
        }
    }
    // The filter goes in while still root with CAP_SYS_ADMIN (no need for
    // no_new_privs, which would break setuid binaries in the container).
    if seccomp {
        if let Err(e) = crate::seccomp::install(caps) {
            return format!("seccomp: {e}");
        }
    }
    let gids: Vec<libc::gid_t> = groups.iter().map(|g| *g as libc::gid_t).collect();
    // SAFETY: valid group list; plain credential syscalls.
    unsafe {
        if libc::setgroups(gids.len(), gids.as_ptr()) < 0 {
            return format!("setgroups: {}", io::Error::last_os_error());
        }
        if libc::setgid(gid) < 0 {
            return format!("setgid {gid}: {}", io::Error::last_os_error());
        }
        if libc::setuid(uid) < 0 {
            return format!("setuid {uid}: {}", io::Error::last_os_error());
        }
    }
    if uid == 0 {
        // Root keeps exactly the container's capability set.
        let mut data = [CapData::default(); 2];
        for &cap in caps {
            let (i, bit) = ((cap / 32) as usize, 1u32 << (cap % 32));
            data[i].effective |= bit;
            data[i].permitted |= bit;
            data[i].inheritable |= bit;
        }
        let mut hdr = CapHeader { version: 0x2008_0522, pid: 0 };
        // SAFETY: header and two data structs, as capset v3 expects.
        if unsafe { libc::syscall(libc::SYS_capset, &mut hdr as *mut CapHeader, data.as_ptr()) } < 0 {
            return format!("capset: {}", io::Error::last_os_error());
        }
    }
    // SAFETY: plain prctl/getppid; die with the helper so a dead daemon
    // cannot leave unmanaged containers behind.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0);
    }
    // Signals: defaults and an empty mask for the container command.
    // SAFETY: plain signal syscalls.
    unsafe {
        for s in 1..32 {
            if s != libc::SIGKILL && s != libc::SIGSTOP {
                libc::signal(s, libc::SIG_DFL);
            }
        }
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
        libc::setsid();
        if tty {
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
        }
    }
    if std::env::set_current_dir(&p.cwd).is_err() {
        let _ = fs::create_dir_all(&p.cwd);
        if let Err(e) = std::env::set_current_dir(&p.cwd) {
            return format!("chdir to cwd (\"{}\") set in config.json failed: {e}", p.cwd);
        }
    }
    let mut argv: Vec<String> = Vec::new();
    if init {
        argv.extend(["/sbin/docker-init".to_string(), "--".to_string()]);
    }
    argv.push(p.path.clone());
    argv.extend(p.args.iter().cloned());
    let program = if init { argv[0].clone() } else { p.path.clone() };
    let path_env = p.env.iter().find_map(|e| e.strip_prefix("PATH=")).unwrap_or("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    let resolved = if program.contains('/') {
        Some(PathBuf::from(&program))
    } else {
        path_env.split(':').map(|d| Path::new(d).join(&program)).find(|c| {
            fs::metadata(c).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    };
    let Some(resolved) = resolved else {
        return format!("exec: \"{program}\": executable file not found in $PATH");
    };
    let cargv: Vec<CString> = argv.iter().map(|a| CString::new(a.as_str()).unwrap_or_default()).collect();
    let cenv: Vec<CString> = p.env.iter().map(|e| CString::new(e.as_str()).unwrap_or_default()).collect();
    let mut pargv: Vec<*const libc::c_char> = cargv.iter().map(|a| a.as_ptr()).collect();
    pargv.push(std::ptr::null());
    let mut penv: Vec<*const libc::c_char> = cenv.iter().map(|e| e.as_ptr()).collect();
    penv.push(std::ptr::null());
    let cpath = c(&resolved);
    // SAFETY: NULL-terminated argv/envp of valid C strings.
    unsafe { libc::execve(cpath.as_ptr(), pargv.as_ptr(), penv.as_ptr()) };
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOENT) => format!("exec: \"{}\": no such file or directory", resolved.display()),
        Some(libc::EACCES) => format!("exec: \"{}\": permission denied", resolved.display()),
        _ => format!("exec {}: {e}", resolved.display()),
    }
}

/// fd 3: line protocol with the daemon.
fn control() -> UnixStream {
    // SAFETY: the daemon passes the socket as fd 3.
    unsafe { UnixStream::from_raw_fd(3) }
}

fn send(ctl: &mut UnixStream, line: &str) {
    let _ = ctl.write_all(format!("{line}\n").as_bytes());
}

fn cloexec_pipe() -> io::Result<(File, File)> {
    let mut fds = [0; 2];
    // SAFETY: fds is a valid out array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptors.
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

/// Fork the container process; `child` runs in the child and returns an
/// error message only if it failed (on success it execs). Then report to the
/// daemon and wait. Never returns.
fn fork_and_wait(ctl: &mut UnixStream, child: impl FnOnce() -> String) -> ! {
    let (mut err_r, err_w) = match cloexec_pipe() {
        Ok(p) => p,
        Err(e) => {
            send(ctl, &format!("ERR pipe: {e}"));
            std::process::exit(126);
        }
    };
    // SAFETY: this process is single threaded.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        send(ctl, &format!("ERR fork: {}", io::Error::last_os_error()));
        std::process::exit(126);
    }
    if pid == 0 {
        drop(err_r);
        // SAFETY: fd 3 is the control socket; the child must not hold it.
        unsafe { libc::close(3) };
        let msg = child();
        let mut w = err_w;
        let _ = w.write_all(msg.as_bytes());
        // SAFETY: skip atexit handlers in the forked child.
        unsafe { libc::_exit(127) };
    }
    drop(err_w);
    let mut msg = String::new();
    let _ = err_r.read_to_string(&mut msg);
    if !msg.is_empty() {
        send(ctl, &format!("ERR {}", msg.replace('\n', " ")));
        let mut st = 0;
        // SAFETY: valid out pointer.
        unsafe { libc::waitpid(pid, &mut st, 0) };
        std::process::exit(127);
    }
    send(ctl, &format!("PID {pid}"));
    loop {
        let mut st = 0;
        // SAFETY: valid out pointer.
        let r = unsafe { libc::waitpid(pid, &mut st, 0) };
        if r == pid {
            let code = if libc::WIFEXITED(st) {
                libc::WEXITSTATUS(st)
            } else if libc::WIFSIGNALED(st) {
                128 + libc::WTERMSIG(st)
            } else {
                continue;
            };
            std::process::exit(code);
        }
        if r < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            std::process::exit(255);
        }
    }
}

/// Ignore the signals a terminal or the daemon's process group might send:
/// the helper only exits when the container does.
fn detach_helper() {
    // SAFETY: plain signal/session syscalls.
    unsafe {
        libc::setsid();
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT, libc::SIGPIPE] {
            libc::signal(s, libc::SIG_IGN);
        }
    }
}

fn read_spec<T: for<'de> Deserialize<'de>>(path: Option<&String>) -> std::result::Result<T, String> {
    let path = path.ok_or("usage: pios container-init|container-exec SPEC")?;
    let text = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    serde_json::from_slice(&text).map_err(|e| format!("{path}: {e}"))
}

fn configure_network(spec: &RuntimeSpec) -> std::result::Result<(), String> {
    let e = |what: &str, err: io::Error| format!("{what}: {err}");
    netlink::set_up("lo").map_err(|x| e("lo up", x))?;
    if spec.net != NetMode::Bridge {
        return Ok(());
    }
    let ip = spec.ip.ok_or("bridge network without an address")?;
    if let Some(mac) = netlink::parse_mac(&spec.mac) {
        netlink::set_mac("eth0", mac).map_err(|x| e("eth0 mac", x))?;
    }
    netlink::add_address("eth0", ip, spec.prefix).map_err(|x| e("eth0 address", x))?;
    netlink::set_up("eth0").map_err(|x| e("eth0 up", x))?;
    netlink::add_default_route("eth0", spec.gateway).map_err(|x| e("default route", x))
}

/// `pios container-init SPEC`.
pub fn container_init(args: &[String]) -> i32 {
    let mut ctl = control();
    detach_helper();
    let spec: RuntimeSpec = match read_spec(args.first()) {
        Ok(s) => s,
        Err(e) => {
            send(&mut ctl, &format!("ERR {e}"));
            return 126;
        }
    };
    let setup = (|| -> std::result::Result<(), String> {
        let mut flags = libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC | libc::CLONE_NEWPID | libc::CLONE_NEWCGROUP;
        if spec.net != NetMode::Host {
            flags |= libc::CLONE_NEWNET;
        }
        // SAFETY: plain syscall in a single threaded process.
        if unsafe { libc::unshare(flags) } < 0 {
            return errno("unshare");
        }
        // Let the daemon wire the veth into our network namespace.
        send(&mut ctl, "NS");
        let mut line = String::new();
        BufReader::new(&ctl).read_line(&mut line).map_err(|e| format!("control: {e}"))?;
        if line.trim() != "GO" {
            return Err(format!("daemon: {}", line.trim()));
        }
        configure_network(&spec)?;
        let h = spec.hostname.as_bytes();
        // SAFETY: pointer/len of a valid slice.
        if unsafe { libc::sethostname(h.as_ptr().cast(), h.len()) } < 0 {
            return errno("sethostname");
        }
        mount("", Path::new("/"), "", libc::MS_REC | libc::MS_PRIVATE, "")?;
        // Relative lowerdirs keep the option string short for deep images.
        std::env::set_current_dir(&spec.layers_dir).map_err(|e| format!("chdir {}: {e}", spec.layers_dir))?;
        if spec.lower.is_empty() {
            return Err("image has no layers".into());
        }
        let opts = format!("lowerdir={},upperdir={},workdir={}", spec.lower.join(":"), spec.upper, spec.work);
        if opts.len() > 4000 {
            return Err(format!("image has too many layers ({})", spec.lower.len()));
        }
        mount("overlay", Path::new(&spec.rootfs), "overlay", 0, &opts)?;
        std::env::set_current_dir("/").map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(e) = setup {
        send(&mut ctl, &format!("ERR {e}"));
        return 126;
    }
    let init = spec.init.is_some();
    fork_and_wait(&mut ctl, || {
        if let Err(e) = setup_root(&spec) {
            return e;
        }
        exec_process(&spec.process, spec.tty, &spec.caps, spec.seccomp, &spec.rlimits, init)
    })
}

/// `pios container-exec SPEC`: join a running container and run a command.
pub fn container_exec(args: &[String]) -> i32 {
    let mut ctl = control();
    detach_helper();
    let spec: ExecSpec = match read_spec(args.first()) {
        Ok(s) => s,
        Err(e) => {
            send(&mut ctl, &format!("ERR {e}"));
            return 126;
        }
    };
    let joined = (|| -> std::result::Result<(), String> {
        // Open every namespace before entering any: after the mount
        // namespace, /proc/<pid> would be the container's /proc.
        let mut fds = Vec::new();
        for (ns, flag) in [
            ("ipc", libc::CLONE_NEWIPC),
            ("uts", libc::CLONE_NEWUTS),
            ("net", libc::CLONE_NEWNET),
            ("pid", libc::CLONE_NEWPID),
            ("cgroup", libc::CLONE_NEWCGROUP),
            ("mnt", libc::CLONE_NEWNS),
        ] {
            let f = File::open(format!("/proc/{}/ns/{ns}", spec.pid)).map_err(|e| format!("container namespace {ns}: {e}"))?;
            fds.push((f, flag, ns));
        }
        for (f, flag, ns) in &fds {
            // SAFETY: valid namespace descriptor.
            if unsafe { libc::setns(f.as_raw_fd(), *flag) } < 0 {
                return errno(format!("setns {ns}"));
            }
        }
        Ok(())
    })();
    if let Err(e) = joined {
        send(&mut ctl, &format!("ERR {e}"));
        return 126;
    }
    fork_and_wait(&mut ctl, || exec_process(&spec.process, spec.tty, &spec.caps, spec.seccomp, &spec.rlimits, false))
}

// ---------------------------------------------------------------------------
// Daemon side
// ---------------------------------------------------------------------------

/// Standard streams of a started process, as seen by the daemon.
#[derive(Default)]
pub struct ProcIo {
    /// Writable end of the process stdin (pipe, or the pty master).
    pub stdin: Option<File>,
    pub stdout: Option<File>,
    pub stderr: Option<File>,
    /// pty master (TTY mode): output and window size.
    pub pty: Option<File>,
}

pub struct Spawned {
    /// The helper; its exit status is the container's.
    pub child: Child,
    /// Host PID of the container process.
    pub pid: i32,
    pub io: ProcIo,
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: valid out array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptors.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn openpty() -> io::Result<(File, File)> {
    let master = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC).open("/dev/ptmx")?;
    let mut n: libc::c_uint = 0;
    let mut unlock: libc::c_int = 0;
    // SAFETY: TIOCSPTLCK/TIOCGPTN with valid out pointers on the ptmx fd.
    unsafe {
        if libc::ioctl(master.as_raw_fd(), libc::TIOCSPTLCK as _, &mut unlock) < 0
            || libc::ioctl(master.as_raw_fd(), libc::TIOCGPTN as _, &mut n) < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let slave = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC).open(format!("/dev/pts/{n}"))?;
    Ok((master, slave))
}

pub fn resize(pty: &File, rows: u16, cols: u16) -> io::Result<()> {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: valid winsize for TIOCSWINSZ.
    if unsafe { libc::ioctl(pty.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Start a helper (`container-init` or `container-exec`) on `spec_path`.
/// `cgroup_procs` is the container cgroup's `cgroup.procs`. For
/// `container-init`, `on_ns(helper_pid)` runs once the helper has its new
/// namespaces (network wiring).
pub fn spawn(
    bin: &str,
    subcommand: &str,
    spec_path: &Path,
    cgroup_procs: &File,
    tty: bool,
    open_stdin: bool,
    on_ns: &mut dyn FnMut(u32) -> Result<()>,
) -> Result<Spawned> {
    let (ours, theirs) = UnixStream::pair()?;
    let mut io = ProcIo::default();
    let (stdin, stdout, stderr): (Stdio, Stdio, Stdio);
    if tty {
        let (master, slave) = openpty()?;
        stdin = Stdio::from(slave.try_clone()?);
        stdout = Stdio::from(slave.try_clone()?);
        stderr = Stdio::from(slave);
        if open_stdin {
            io.stdin = Some(master.try_clone()?);
        }
        io.pty = Some(master);
    } else {
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        stdout = Stdio::from(out_w);
        stderr = Stdio::from(err_w);
        io.stdout = Some(File::from(out_r));
        io.stderr = Some(File::from(err_r));
        if open_stdin {
            let (in_r, in_w) = pipe()?;
            stdin = Stdio::from(in_r);
            io.stdin = Some(File::from(in_w));
        } else {
            stdin = Stdio::null();
        }
    }
    let theirs_fd: RawFd = theirs.as_raw_fd();
    let procs_fd: RawFd = cgroup_procs.as_raw_fd();
    let mut cmd = Command::new(bin);
    cmd.arg(subcommand).arg(spec_path).stdin(stdin).stdout(stdout).stderr(stderr).env_clear().env("PATH", pios_core::service::DEFAULT_PATH);
    // SAFETY: only async-signal-safe calls (write, dup2) between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::write(procs_fd, b"0".as_ptr().cast(), 1) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::dup2(theirs_fd, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| Error::msg(format!("spawn {bin} {subcommand}: {e}")))?;
    drop(cmd);
    drop(theirs);
    ours.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut reader = BufReader::new(ours.try_clone()?);
    let mut writer = ours;
    let fail = |child: &mut Child, msg: String| -> Error {
        let _ = child.kill();
        let _ = child.wait();
        Error::msg(msg)
    };
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            let status = child.wait().ok();
            return Err(Error::msg(format!("container helper exited during setup ({status:?})")));
        }
        let l = line.trim();
        if l == "NS" {
            match on_ns(child.id()) {
                Ok(()) => writer.write_all(b"GO\n")?,
                Err(e) => {
                    let _ = writer.write_all(format!("ERR {e}\n").as_bytes());
                    return Err(fail(&mut child, e.to_string()));
                }
            }
        } else if let Some(pid) = l.strip_prefix("PID ") {
            let pid = pid.parse().map_err(|_| Error::msg(format!("bad helper reply {l:?}")))?;
            return Ok(Spawned { child, pid, io });
        } else if let Some(e) = l.strip_prefix("ERR ") {
            let _ = child.wait();
            return Err(Error::msg(e.to_string()));
        } else {
            return Err(fail(&mut child, format!("bad helper reply {l:?}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_join_stays_inside_root() {
        let root = std::env::temp_dir().join(format!("pios-sj-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("abs")).unwrap();
        std::os::unix::fs::symlink("../../real", root.join("rel")).unwrap();
        assert_eq!(secure_join(&root, "/abs/passwd"), root.join("etc/passwd"));
        assert_eq!(secure_join(&root, "/rel/x"), root.join("real/x"));
        assert_eq!(secure_join(&root, "/../../x"), root.join("x"));
        assert_eq!(secure_join(&root, "/new/dir"), root.join("new/dir"));
    }
}
