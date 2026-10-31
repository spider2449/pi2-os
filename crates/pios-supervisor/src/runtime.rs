//! Event loop: one `poll(2)` over a signalfd, the control socket and the
//! services' output pipes. Single threaded, no async runtime, tiny RSS.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pios_core::ipc::{Request, Response, ServiceStatus};
use pios_core::service::{ReadyCheck, RestartPolicy, ServiceSpec};
use pios_core::{debug, error, info, warn, Error, Result};

use crate::cgroup::{self, Cgroup};
use crate::ring::Ring;
use crate::{backoff, graph, ready};

const LOG_RING_BYTES: usize = 32 * 1024;
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// Why the supervisor loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Reboot,
    TryReboot,
    Poweroff,
}

#[derive(Debug)]
enum State {
    /// Waiting for dependencies.
    Waiting,
    Running { pid: i32, started: Instant, ready: bool },
    /// Being stopped on request; `restart` re-queues it afterwards.
    Stopping { pid: i32, deadline: Instant, restart: bool },
    Backoff { until: Instant },
    /// Exited successfully and will not be restarted (counts as ready).
    Done,
    Failed,
    /// Stopped by the operator.
    Stopped,
}

impl State {
    fn label(&self) -> &'static str {
        match self {
            State::Waiting => "waiting",
            State::Running { ready: true, .. } => "running",
            State::Running { ready: false, .. } => "starting",
            State::Stopping { .. } => "stopping",
            State::Backoff { .. } => "backoff",
            State::Done => "done",
            State::Failed => "failed",
            State::Stopped => "stopped",
        }
    }

    fn pid(&self) -> Option<i32> {
        match *self {
            State::Running { pid, .. } | State::Stopping { pid, .. } => Some(pid),
            _ => None,
        }
    }
}

struct Unit {
    spec: ServiceSpec,
    state: State,
    restarts: u32,
    failures: u32,
    log: Ring,
    out: Option<OwnedFd>,
    cgroup: Option<Cgroup>,
    on_ready: Option<Box<dyn FnOnce()>>,
}

pub struct Supervisor {
    units: Vec<Unit>,
    order: Vec<usize>,
    sigfd: OwnedFd,
    ctl: Option<UnixListener>,
    console_logs: bool,
    exit: Option<Exit>,
}

impl Supervisor {
    /// Validate the service graph and prepare signal handling. Must be called
    /// before any other thread exists (signal mask is process wide).
    pub fn new(specs: Vec<ServiceSpec>, ctl_socket: Option<&str>, console_logs: bool) -> Result<Self> {
        let order = graph::start_order(&specs)?;
        if let Err(e) = cgroup::init() {
            warn!("supervisor", "cgroup setup failed, running without isolation: {e}");
        }
        let sigfd = signalfd()?;
        let ctl = match ctl_socket {
            Some(path) => {
                let _ = std::fs::remove_file(path);
                if let Some(dir) = std::path::Path::new(path).parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let l = UnixListener::bind(path).map_err(|e| Error::msg(format!("bind {path}: {e}")))?;
                l.set_nonblocking(true)?;
                Some(l)
            }
            None => None,
        };
        let units = specs
            .into_iter()
            .map(|spec| Unit {
                cgroup: Cgroup::create(&spec.name, &spec.limits)
                    .map_err(|e| warn!("supervisor", "{}: no cgroup: {e}", spec.name))
                    .ok(),
                spec,
                state: State::Waiting,
                restarts: 0,
                failures: 0,
                log: Ring::new(LOG_RING_BYTES),
                out: None,
                on_ready: None,
            })
            .collect();
        Ok(Self { units, order, sigfd, ctl, console_logs, exit: None })
    }

    /// Run `hook` once, the first time `service` becomes ready.
    pub fn on_ready(&mut self, service: &str, hook: Box<dyn FnOnce()>) {
        if let Some(u) = self.units.iter_mut().find(|u| u.spec.name == service) {
            u.on_ready = Some(hook);
        }
    }

    /// Run until a reboot/poweroff is requested.
    pub fn run(&mut self) -> Exit {
        loop {
            self.schedule();
            if let Some(exit) = self.exit {
                return exit;
            }
            let timeout = self.poll_timeout();
            let mut fds = vec![libc::pollfd { fd: self.sigfd.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
            if let Some(l) = &self.ctl {
                fds.push(libc::pollfd { fd: l.as_raw_fd(), events: libc::POLLIN, revents: 0 });
            }
            let first_out = fds.len();
            let mut out_owner = Vec::new();
            for (i, u) in self.units.iter().enumerate() {
                if let Some(fd) = &u.out {
                    fds.push(libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 });
                    out_owner.push(i);
                }
            }
            // SAFETY: fds is a valid array of pollfd for its length.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if n <= 0 {
                continue;
            }
            if fds[0].revents != 0 {
                self.handle_signals();
            }
            if self.ctl.is_some() && fds[1].revents != 0 {
                self.handle_ctl();
            }
            for (k, &i) in out_owner.iter().enumerate() {
                if fds[first_out + k].revents != 0 {
                    self.drain_output(i);
                }
            }
        }
    }

    /// Stop everything in reverse dependency order, then remaining orphans.
    pub fn shutdown(&mut self) {
        info!("supervisor", "stopping services");
        for &i in self.order.clone().iter().rev() {
            let Some(pid) = self.units[i].state.pid() else { continue };
            let spec = self.units[i].spec.clone();
            if let Some(hook) = &spec.shutdown_hook {
                info!("supervisor", "{}: running shutdown hook", spec.name);
                match spawn_plain(hook) {
                    Ok(hpid) => {
                        if !wait_for(hpid, spec.stop_timeout) {
                            let _ = pios_core::sys::kill(hpid, libc::SIGKILL);
                        }
                    }
                    Err(e) => warn!("supervisor", "{}: shutdown hook: {e}", spec.name),
                }
            }
            let _ = pios_core::sys::kill(pid, spec.stop_signal);
            if !wait_for(pid, spec.stop_timeout) {
                warn!("supervisor", "{}: did not stop in {:?}, killing", spec.name, spec.stop_timeout);
                let _ = pios_core::sys::kill(-pid, libc::SIGKILL);
                let _ = pios_core::sys::kill(pid, libc::SIGKILL);
                wait_for(pid, Duration::from_secs(2));
            }
            self.units[i].state = State::Stopped;
        }
        // Orphans (e.g. container shims) last.
        let _ = pios_core::sys::kill(-1, libc::SIGTERM);
        wait_all(Duration::from_secs(3));
        let _ = pios_core::sys::kill(-1, libc::SIGKILL);
        wait_all(Duration::from_millis(500));
    }

    fn dep_ready(&self, name: &str) -> bool {
        self.units
            .iter()
            .find(|u| u.spec.name == name)
            .is_some_and(|u| matches!(u.state, State::Running { ready: true, .. } | State::Done))
    }

    /// Advance every unit's state machine based on time and dependencies.
    fn schedule(&mut self) {
        let now = Instant::now();
        for k in 0..self.order.len() {
            let i = self.order[k];
            match self.units[i].state {
                State::Waiting => {
                    let deps_ok = self.units[i].spec.after.iter().all(|d| self.dep_ready(d));
                    if deps_ok {
                        self.start(i);
                    }
                }
                State::Backoff { until } if now >= until => self.start(i),
                State::Running { ready: false, pid, started } => {
                    if ready::probe(&self.units[i].spec.ready) {
                        self.units[i].state = State::Running { pid, started, ready: true };
                        self.mark_ready(i, started);
                    }
                }
                State::Stopping { pid, deadline, .. } if now >= deadline => {
                    warn!("supervisor", "{}: stop timeout, killing", self.units[i].spec.name);
                    let _ = pios_core::sys::kill(-pid, libc::SIGKILL);
                    let _ = pios_core::sys::kill(pid, libc::SIGKILL);
                }
                _ => {}
            }
        }
    }

    fn mark_ready(&mut self, i: usize, started: Instant) {
        let u = &mut self.units[i];
        info!("supervisor", "{} ready ({} ms)", u.spec.name, started.elapsed().as_millis());
        if let Some(hook) = u.on_ready.take() {
            hook();
        }
    }

    fn poll_timeout(&self) -> i32 {
        let now = Instant::now();
        let mut next: Option<Duration> = None;
        let mut consider = |d: Duration| next = Some(next.map_or(d, |n| n.min(d)));
        for u in &self.units {
            match u.state {
                State::Running { ready: false, .. } => consider(PROBE_INTERVAL),
                State::Backoff { until } => consider(until.saturating_duration_since(now)),
                State::Stopping { deadline, .. } => consider(deadline.saturating_duration_since(now)),
                _ => {}
            }
        }
        next.map_or(-1, |d| d.as_millis().clamp(1, i32::MAX as u128) as i32)
    }

    fn start(&mut self, i: usize) {
        let u = &mut self.units[i];
        match spawn_service(&u.spec, u.cgroup.as_ref()) {
            Ok((pid, out)) => {
                debug!("supervisor", "{} started pid {pid}", u.spec.name);
                let started = Instant::now();
                u.out = Some(out);
                let ready_now = matches!(u.spec.ready, ReadyCheck::Started);
                u.state = State::Running { pid, started, ready: ready_now };
                if ready_now {
                    self.mark_ready(i, started);
                }
            }
            Err(e) => {
                error!("supervisor", "{}: spawn failed: {e}", u.spec.name);
                let (delay, failures) = backoff::next_delay(u.failures, Duration::ZERO);
                u.failures = failures;
                u.state = State::Backoff { until: Instant::now() + delay };
            }
        }
    }

    fn handle_signals(&mut self) {
        let mut info = [0u8; std::mem::size_of::<libc::signalfd_siginfo>()];
        loop {
            // SAFETY: reading into a correctly sized buffer.
            let n = unsafe { libc::read(self.sigfd.as_raw_fd(), info.as_mut_ptr().cast(), info.len()) };
            if n != info.len() as isize {
                break;
            }
            // SAFETY: the kernel filled a complete signalfd_siginfo.
            let si: libc::signalfd_siginfo = unsafe { std::ptr::read_unaligned(info.as_ptr().cast()) };
            match si.ssi_signo as i32 {
                libc::SIGCHLD => {}
                libc::SIGTERM | libc::SIGINT => self.exit = Some(Exit::Reboot),
                libc::SIGUSR2 | libc::SIGPWR => self.exit = Some(Exit::Poweroff),
                other => debug!("supervisor", "ignoring signal {other}"),
            }
        }
        // Always reap: SIGCHLD instances coalesce.
        while let Some((pid, status)) = reap(libc::WNOHANG) {
            self.on_exit(pid, status);
        }
    }

    fn on_exit(&mut self, pid: i32, status: i32) {
        let Some(i) = self.units.iter().position(|u| u.state.pid() == Some(pid)) else {
            debug!("supervisor", "reaped orphan {pid}");
            return;
        };
        self.drain_output(i);
        let u = &mut self.units[i];
        u.out = None;
        let code = exit_code(status);
        match u.state {
            State::Stopping { restart, .. } => {
                info!("supervisor", "{} stopped", u.spec.name);
                u.state = if restart { State::Waiting } else { State::Stopped };
            }
            State::Running { started, .. } => {
                let ran = started.elapsed();
                let success = code == 0;
                let restart = match u.spec.restart {
                    RestartPolicy::Always => true,
                    RestartPolicy::OnFailure => !success,
                    RestartPolicy::Never => false,
                };
                if restart {
                    let (delay, failures) = backoff::next_delay(u.failures, ran);
                    u.failures = failures;
                    u.restarts += 1;
                    warn!("supervisor", "{} exited ({code}) after {ran:?}, restart in {delay:?}", u.spec.name);
                    u.state = State::Backoff { until: Instant::now() + delay };
                } else if success {
                    info!("supervisor", "{} completed", u.spec.name);
                    u.state = State::Done;
                    if matches!(u.spec.ready, ReadyCheck::Exited) {
                        self.mark_ready(i, started);
                    }
                } else {
                    error!("supervisor", "{} failed ({code})", u.spec.name);
                    u.state = State::Failed;
                }
            }
            _ => {}
        }
    }

    fn drain_output(&mut self, i: usize) {
        let u = &mut self.units[i];
        let Some(fd) = &u.out else { return };
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: reading into a stack buffer from an owned, non-blocking fd.
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                if n == 0 {
                    u.out = None; // EOF: writer side fully closed.
                }
                break;
            }
            let chunk = &buf[..n as usize];
            u.log.push(chunk);
            if self.console_logs {
                let mut err = std::io::stderr().lock();
                for line in chunk.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
                    let _ = write!(err, "[{}] ", u.spec.name);
                    let _ = err.write_all(line);
                    let _ = err.write_all(b"\n");
                }
            }
        }
    }

    fn handle_ctl(&mut self) {
        let Some(listener) = &self.ctl else { return };
        let mut pending = Vec::new();
        while let Ok((stream, _)) = listener.accept() {
            pending.push(stream);
        }
        for stream in pending {
            let resp = match read_request(&stream) {
                Ok(req) => self.dispatch(req),
                Err(e) => Response::Error { message: e },
            };
            let mut stream = stream;
            let _ = serde_json::to_writer(&mut stream, &resp);
            let _ = stream.write_all(b"\n");
        }
    }

    fn dispatch(&mut self, req: Request) -> Response {
        let find = |units: &[Unit], name: &str| units.iter().position(|u| u.spec.name == name);
        match req {
            Request::Status => Response::Status {
                uptime_secs: pios_core::sys::uptime_secs(),
                services: self
                    .order
                    .iter()
                    .map(|&i| {
                        let u = &self.units[i];
                        ServiceStatus {
                            name: u.spec.name.clone(),
                            state: u.state.label().into(),
                            pid: u.state.pid(),
                            restarts: u.restarts,
                        }
                    })
                    .collect(),
            },
            Request::Logs { service } => match find(&self.units, &service) {
                Some(i) => Response::Logs { service, text: self.units[i].log.text() },
                None => unknown(&service),
            },
            Request::Start { service } => match find(&self.units, &service) {
                Some(i) => {
                    let u = &mut self.units[i];
                    if matches!(u.state, State::Stopped | State::Failed | State::Done) {
                        u.failures = 0;
                        u.state = State::Waiting;
                    }
                    Response::Ok
                }
                None => unknown(&service),
            },
            Request::Stop { service } => self.request_stop(&service, false),
            Request::Restart { service } => self.request_stop(&service, true),
            Request::TryReboot => {
                self.exit = Some(Exit::TryReboot);
                Response::Ok
            }
            Request::Reboot => {
                self.exit = Some(Exit::Reboot);
                Response::Ok
            }
            Request::Poweroff => {
                self.exit = Some(Exit::Poweroff);
                Response::Ok
            }
        }
    }

    fn request_stop(&mut self, service: &str, restart: bool) -> Response {
        let Some(u) = self.units.iter_mut().find(|u| u.spec.name == service) else {
            return unknown(service);
        };
        match u.state {
            State::Running { pid, .. } => {
                let _ = pios_core::sys::kill(pid, u.spec.stop_signal);
                u.state = State::Stopping { pid, deadline: Instant::now() + u.spec.stop_timeout, restart };
            }
            State::Stopping { pid, deadline, .. } => u.state = State::Stopping { pid, deadline, restart },
            _ => {
                u.failures = 0;
                u.state = if restart { State::Waiting } else { State::Stopped };
            }
        }
        Response::Ok
    }
}

fn unknown(name: &str) -> Response {
    Response::Error { message: format!("unknown service '{name}'") }
}

fn read_request(stream: &UnixStream) -> std::result::Result<Request, String> {
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(1))).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| format!("bad request: {e}"))
}

/// Block the supervised signals and receive them through a signalfd instead.
fn signalfd() -> Result<OwnedFd> {
    // SAFETY: sigset manipulation on a local, zero-initialised set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT, libc::SIGUSR1, libc::SIGUSR2, libc::SIGPWR] {
            libc::sigaddset(&mut set, s);
        }
        if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let fd = libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

/// Child-side setup shared by services and hooks: clean signal mask, own session.
fn child_setup() -> std::io::Result<()> {
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
        libc::setsid();
    }
    Ok(())
}

fn spawn_service(spec: &ServiceSpec, cg: Option<&Cgroup>) -> Result<(i32, OwnedFd)> {
    let (rd, wr) = pipe()?;
    let wr2 = wr.try_clone()?;
    let mut cmd = Command::new(&spec.exec[0]);
    cmd.args(&spec.exec[1..])
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::from(wr))
        .stderr(Stdio::from(wr2))
        .current_dir("/");
    let procs_fd: RawFd = cg.map_or(-1, |c| c.procs_fd());
    // SAFETY: the closure only performs async-signal-safe syscalls.
    unsafe {
        cmd.pre_exec(move || {
            child_setup()?;
            if procs_fd >= 0 {
                libc::write(procs_fd, b"0".as_ptr().cast(), 1);
            }
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| Error::msg(format!("exec {}: {e}", spec.exec[0])))?;
    // Never wait on `child`: PID 1 reaps everything via waitpid(-1).
    Ok((child.id() as i32, rd))
}

fn spawn_plain(argv: &[String]) -> Result<i32> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::null());
    // SAFETY: async-signal-safe setup only.
    unsafe { cmd.pre_exec(child_setup) };
    Ok(cmd.spawn().map_err(|e| Error::msg(format!("exec {}: {e}", argv[0])))?.id() as i32)
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: fds is a valid 2-element array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: pipe2 returned two fresh descriptors we now own.
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: fcntl on an owned descriptor.
    unsafe { libc::fcntl(rd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    Ok((rd, wr))
}

fn reap(flags: i32) -> Option<(i32, i32)> {
    let mut status = 0;
    // SAFETY: status is a valid out-pointer.
    let pid = unsafe { libc::waitpid(-1, &mut status, flags) };
    (pid > 0).then_some((pid, status))
}

fn exit_code(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    }
}

/// Reap children until `pid` exits or the timeout elapses.
fn wait_for(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        while let Some((p, _)) = reap(libc::WNOHANG) {
            if p == pid {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Reap until no children remain or the timeout elapses.
fn wait_all(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let mut status = 0;
        // SAFETY: status is a valid out-pointer.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid < 0 {
            return; // ECHILD: nothing left.
        }
        if pid == 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
