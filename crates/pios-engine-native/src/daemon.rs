//! `pios engined`: the native engine daemon. Serves the subset of the Docker
//! Engine API the docker CLI needs for everyday work (run, ps, logs, exec,
//! stop, rm, pull, images, volumes) on /var/run/docker.sock and, with
//! certificates, on TCP 2376 with mutual TLS. Thread per connection, no async
//! runtime; container processes are its direct children (see `runtime`).

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use pios_core::config::{EngineConfig, PiosConfig};
use pios_core::{error, info, paths, warn, Error, Result};

use crate::client::Client;
use crate::http::{self, mux_frame, read_request, respond, Request, API_HEADERS};
use crate::logs::{self, LogWriter};
use crate::netmodel::{allocate_subnet, Filters, Network, NetworkCreate};
use crate::network::{self, GATEWAY, PREFIX, SUBNET};
use crate::reference::Reference;
use crate::registry::{self, Auth};
use crate::runtime::{self, ExecSpec, RuntimeSpec, SpecMount};
use crate::state::*;
use crate::store::{write_atomic, Store};
use crate::util::{now_ns, random_id, random_name, rfc3339, signal_number};
use crate::DOCKER_SOCKET;

/// API error: HTTP status and message (`{"message": ...}`).
#[derive(Debug)]
pub struct ApiError(u16, String);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError(500, e.to_string())
    }
}

impl From<io::Error> for ApiError {
    fn from(e: io::Error) -> Self {
        ApiError(500, e.to_string())
    }
}

type ApiResult<T = Value> = std::result::Result<T, ApiError>;

fn not_found(what: &str) -> ApiError {
    ApiError(404, format!("No such container: {what}"))
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(400, msg.into())
}

type Chunk = (u8, Arc<Vec<u8>>);

/// Live, non-persisted state of a container.
#[derive(Default)]
struct Live {
    subscribers: Vec<mpsc::Sender<Chunk>>,
    stdin: Option<File>,
    pty: Option<File>,
}

struct Ctr {
    c: Mutex<Container>,
    /// Signalled (with `c` held) on every state change.
    cv: Condvar,
    live: Mutex<Live>,
    exits: AtomicU64,
    removed: AtomicBool,
    dir: PathBuf,
}

impl Ctr {
    fn lock(&self) -> MutexGuard<'_, Container> {
        self.c.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, c: &Container) {
        let data = serde_json::to_vec_pretty(c).expect("json");
        if let Err(e) = write_atomic(&self.dir.join("container.json"), &data) {
            warn!("engine", "save {}: {e}", c.short_id());
        }
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join("container.log")
    }

    /// Block until `done(container)` or the timeout; returns whether done.
    fn wait_until(&self, timeout: Duration, mut done: impl FnMut(&Container, &Ctr) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut c = self.lock();
        loop {
            if done(&c, self) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            c = self.cv.wait_timeout(c, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// The network a container uses, resolved (default bridge, host, none or
/// a user-defined network).
struct NetRef {
    name: String,
    id: String,
    gateway: Ipv4Addr,
    subnet: Ipv4Addr,
    prefix: u8,
    bridge: String,
}

impl NetRef {
    fn info(&self) -> NetInfo<'_> {
        NetInfo { name: &self.name, id: &self.id, gateway: self.gateway, prefix: self.prefix }
    }
}

struct ExecRec {
    id: String,
    container: String,
    cmd: Vec<String>,
    env: Vec<String>,
    user: String,
    cwd: String,
    tty: bool,
    privileged: bool,
    attach_stdin: bool,
    attach_stdout: bool,
    attach_stderr: bool,
    running: AtomicBool,
    pid: Mutex<i32>,
    exit_code: Mutex<Option<i32>>,
    pty: Mutex<Option<File>>,
}

pub struct Daemon {
    root: PathBuf,
    store: Store,
    client: Option<Client>,
    cfg: EngineConfig,
    hostname: String,
    containers: Mutex<BTreeMap<String, Arc<Ctr>>>,
    execs: Mutex<HashMap<String, Arc<ExecRec>>>,
    /// Serialises starts (IP and host port allocation) and creates (names).
    networks: Mutex<Vec<Network>>,
    alloc: Mutex<()>,
    shutting_down: AtomicBool,
    log_max: u64,
    log_files: u32,
}

impl Daemon {
    fn containers_dir(&self) -> PathBuf {
        self.root.join("containers")
    }

    fn volumes_dir(&self) -> PathBuf {
        self.root.join("volumes")
    }

    fn all(&self) -> Vec<Arc<Ctr>> {
        self.containers.lock().unwrap().values().cloned().collect()
    }

    /// By full id, unique id prefix or name.
    fn find(&self, key: &str) -> ApiResult<Arc<Ctr>> {
        let key = key.trim_start_matches('/');
        // Never lock a container while holding the map: other paths take a
        // container lock first and the map second.
        let map: Vec<(String, Arc<Ctr>)> = self.containers.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        if let Some((_, c)) = map.iter().find(|(id, _)| id == key) {
            return Ok(c.clone());
        }
        if let Some((_, c)) = map.iter().find(|(_, c)| c.lock().name == key) {
            return Ok(c.clone());
        }
        let matches: Vec<&Arc<Ctr>> = map.iter().filter(|(id, _)| id.starts_with(key)).map(|(_, c)| c).collect();
        match matches.len() {
            1 if !key.is_empty() => Ok(matches[0].clone()),
            0 | 1 => Err(not_found(key)),
            _ => Err(bad_request(format!("multiple IDs found with provided prefix: {key}"))),
        }
    }

    fn load_containers(&self) {
        let Ok(rd) = fs::read_dir(self.containers_dir()) else { return };
        for e in rd.flatten() {
            let dir = e.path();
            let Some(mut c) = fs::read(dir.join("container.json")).ok().and_then(|b| serde_json::from_slice::<Container>(&b).ok()) else {
                warn!("engine", "ignoring unreadable container {}", dir.display());
                continue;
            };
            if c.running() || c.state.status == Status::Removing {
                // The previous daemon died with the container: Docker reports 255.
                c.state.status = Status::Exited;
                c.state.exit_code = 255;
                c.state.pid = 0;
                c.state.finished_at = now_ns();
                c.ip = None;
            }
            let ctr = Arc::new(Ctr {
                c: Mutex::new(c.clone()),
                cv: Condvar::new(),
                live: Mutex::new(Live::default()),
                exits: AtomicU64::new(0),
                removed: AtomicBool::new(false),
                dir,
            });
            ctr.save(&c);
            self.containers.lock().unwrap().insert(c.id.clone(), ctr);
        }
    }

    // ---------------------------------------------------------------- create

    fn create(&self, req: &Request) -> ApiResult<(u16, Value)> {
        let body: CreateRequest = serde_json::from_slice(if req.body.is_empty() { b"{}" } else { &req.body })
            .map_err(|e| bad_request(format!("invalid create request: {e}")))?;
        let image = self.store.resolve(&body.image).ok_or_else(|| ApiError(404, format!("No such image: {}", display_image(&body.image))))?;
        let process = process_config(&body, &image.run_config()).map_err(bad_request)?;
        let mode = body.host_config.network_mode.clone();
        let custom = self.find_network(&mode);
        let net = match &custom {
            Some(_) => NetMode::Bridge,
            None => NetMode::parse(&mode).map_err(|_| ApiError(404, format!("network {mode} not found")))?,
        };
        let aliases: Vec<String> = custom
            .as_ref()
            .and_then(|n| {
                let eps = body.networking_config.as_ref()?.endpoints_config.as_ref()?;
                eps.get(&n.name).or_else(|| eps.get(&n.id))?.aliases.clone()
            })
            .unwrap_or_default();
        let mut ports = match &body.host_config.port_bindings {
            Some(pb) if net == NetMode::Bridge => parse_port_bindings(pb).map_err(bad_request)?,
            _ => Vec::new(),
        };
        if body.host_config.publish_all_ports && net == NetMode::Bridge {
            for k in body.exposed_ports.iter().flat_map(|m| m.keys()).chain(image.run_config().exposed_ports.iter().flat_map(|m| m.keys())) {
                let (port, proto) = k.split_once('/').unwrap_or((k, "tcp"));
                let proto = if proto == "udp" { Proto::Udp } else { Proto::Tcp };
                if let Ok(container_port) = port.parse() {
                    if !ports.iter().any(|p| p.container_port == container_port && p.proto == proto) {
                        ports.push(PortMap { proto, container_port, host_ip: None, host_port: 0 });
                    }
                }
            }
        }
        let mut mounts = resolve_mounts(&body.host_config).map_err(bad_request)?;
        for m in &mut mounts {
            if m.kind == MountKind::Volume && m.source.is_empty() {
                m.source = random_id();
            }
        }
        // Image VOLUMEs not covered by a mount get anonymous volumes.
        for v in image.run_config().volumes.iter().flat_map(|m| m.keys()) {
            let target = clean_path(v);
            if !mounts.iter().any(|m| m.target == target) {
                mounts.push(MountPoint { kind: MountKind::Volume, source: random_id(), target, read_only: false });
            }
        }
        capabilities(&body.host_config).map_err(bad_request)?;
        for d in body.host_config.devices.iter().flatten() {
            if !Path::new(&d.path_on_host).exists() {
                return Err(bad_request(format!("error gathering device information while adding custom device {:?}: no such file or directory", d.path_on_host)));
            }
        }

        let _g = self.alloc.lock().unwrap();
        let id = random_id();
        let name = match req.query("name").filter(|n| !n.is_empty()) {
            Some(n) => {
                let n = n.trim_start_matches('/');
                if !valid_container_name(n) {
                    return Err(bad_request(format!("Invalid container name ({n}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed")));
                }
                if let Some(other) = self.all().iter().find(|c| c.lock().name == n) {
                    return Err(ApiError(409, format!(
                        "Conflict. The container name \"/{n}\" is already in use by container \"{}\". You have to remove (or rename) that container to be able to reuse that name.",
                        other.lock().id
                    )));
                }
                n.to_string()
            }
            None => {
                let mut n = random_name(&id);
                if self.all().iter().any(|c| c.lock().name == n) {
                    n = format!("{n}_{}", &id[..4]);
                }
                n
            }
        };
        let hostname = if body.hostname.is_empty() { id[..12].to_string() } else { body.hostname.clone() };
        let dir = self.containers_dir().join(&id);
        for d in ["upper", "work", "merged"] {
            fs::create_dir_all(dir.join(d))?;
        }
        let c = Container {
            id: id.clone(),
            name,
            created: now_ns(),
            image: body.image.clone(),
            image_id: image.id.clone(),
            layers: image.meta.layers.clone(),
            process,
            hostname,
            net,
            ports,
            mounts,
            state: State::default(),
            restart_count: 0,
            manually_stopped: false,
            ip: None,
            network: custom.map(|n| n.id),
            aliases,
            request: body,
        };
        let ctr = Arc::new(Ctr {
            c: Mutex::new(c.clone()),
            cv: Condvar::new(),
            live: Mutex::new(Live::default()),
            exits: AtomicU64::new(0),
            removed: AtomicBool::new(false),
            dir,
        });
        ctr.save(&c);
        self.containers.lock().unwrap().insert(id.clone(), ctr);
        info!("engine", "created {} ({})", &id[..12], c.name);
        Ok((201, json!({"Id": id, "Warnings": []})))
    }

    // ----------------------------------------------------------------- start

    fn volume_path(&self, name: &str) -> PathBuf {
        self.volumes_dir().join(name).join("_data")
    }

    /// Start a created or exited container. Ok(false) if already running.
    fn start(self: &Arc<Self>, ctr: &Arc<Ctr>) -> ApiResult<bool> {
        let _g = self.alloc.lock().unwrap();
        let mut c = ctr.lock();
        if c.state.status == Status::Running {
            return Ok(false);
        }
        if ctr.removed.load(Ordering::SeqCst) {
            return Err(not_found(&c.id));
        }
        if let Some(l) = c.layers.iter().find(|l| !self.store.has_layer(l)) {
            return Err(ApiError(500, format!("image layer {} is missing; pull {} again", &l[..12], c.image)));
        }
        let nr = self.net_ref(&c);
        let ip = if c.net == NetMode::Bridge {
            let mut used = vec![nr.gateway];
            used.extend(self.all().iter().filter(|o| !Arc::ptr_eq(o, ctr)).filter_map(|o| {
                let o = o.lock();
                if o.network == c.network { o.ip } else { None }
            }));
            Some(allocate_ip(nr.subnet, nr.prefix, &used).ok_or_else(|| ApiError(500, format!("no free address on network {}", nr.name)))?)
        } else {
            None
        };
        let taken: Vec<u16> = self.all().iter().filter(|o| !Arc::ptr_eq(o, ctr)).flat_map(|o| o.lock().ports.iter().map(|p| p.host_port).collect::<Vec<_>>()).collect();
        let mut ports = c.ports.clone();
        for p in &mut ports {
            if p.host_port == 0 {
                p.host_port = network::free_port(p.proto, &taken)?;
            }
        }

        // /etc/hostname, /etc/hosts, /etc/resolv.conf
        fs::write(ctr.dir.join("hostname"), format!("{}\n", c.hostname))?;
        let extra = c.request.host_config.extra_hosts.clone().unwrap_or_default();
        fs::write(ctr.dir.join("hosts"), hosts_file(&c.hostname, ip, &extra))?;
        let host_resolv = fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
        let dns = c.request.host_config.dns.clone().unwrap_or_default();
        let resolv = if c.net == NetMode::Host && dns.is_empty() {
            host_resolv
        } else if c.network.is_some() && dns.is_empty() {
            // User-defined network: the embedded DNS on the gateway resolves
            // container names and forwards the rest.
            resolv_conf(&host_resolv, &[nr.gateway.to_string()])
        } else {
            resolv_conf(&host_resolv, &dns)
        };
        fs::write(ctr.dir.join("resolv.conf"), resolv)?;

        let mut mounts = Vec::new();
        for m in &c.mounts {
            match m.kind {
                MountKind::Bind => {
                    if !Path::new(&m.source).exists() {
                        // `-v /host/path:...` creates a missing directory, as in Docker.
                        fs::create_dir_all(&m.source).map_err(|e| bad_request(format!("bind source {}: {e}", m.source)))?;
                    }
                    mounts.push(SpecMount::Bind { source: m.source.clone(), target: m.target.clone(), read_only: m.read_only });
                }
                MountKind::Volume => {
                    let p = self.volume_path(&m.source);
                    fs::create_dir_all(&p)?;
                    mounts.push(SpecMount::Bind { source: p.display().to_string(), target: m.target.clone(), read_only: m.read_only });
                }
                MountKind::Tmpfs => mounts.push(SpecMount::Tmpfs { options: m.source.clone(), target: m.target.clone() }),
            }
        }
        let has = |t: &str| c.mounts.iter().any(|m| m.target == t);
        for (file, target) in [("hostname", "/etc/hostname"), ("hosts", "/etc/hosts"), ("resolv.conf", "/etc/resolv.conf")] {
            if !has(target) {
                mounts.push(SpecMount::Bind { source: ctr.dir.join(file).display().to_string(), target: target.into(), read_only: false });
            }
        }
        let h = &c.request.host_config;
        let mut devices = h.devices.clone().unwrap_or_default();
        for d in &mut devices {
            if d.path_in_container.is_empty() {
                d.path_in_container = d.path_on_host.clone();
            }
        }
        let mut rlimits = vec![("nofile".to_string(), 1_048_576u64, 1_048_576u64)];
        for u in h.ulimits.iter().flatten() {
            rlimits.retain(|(n, _, _)| *n != u.name);
            rlimits.push((u.name.clone(), u.soft.max(0) as u64, u.hard.max(0) as u64));
        }
        let init = (h.init == Some(true)).then(|| "/usr/bin/docker-init".to_string()).filter(|p| Path::new(p).exists());
        let spec = RuntimeSpec {
            layers_dir: self.store.layers_dir().display().to_string(),
            lower: c.layers.iter().rev().cloned().collect(),
            upper: ctr.dir.join("upper").display().to_string(),
            work: ctr.dir.join("work").display().to_string(),
            rootfs: ctr.dir.join("merged").display().to_string(),
            readonly_rootfs: h.readonly_rootfs,
            hostname: c.hostname.clone(),
            process: c.process.clone(),
            tty: c.request.tty,
            net: c.net,
            ip,
            prefix: nr.prefix,
            gateway: nr.gateway,
            mac: ip.map(mac_for).unwrap_or_default(),
            mounts,
            devices,
            privileged: h.privileged,
            caps: capabilities(h).map_err(bad_request)?,
            seccomp: seccomp_enabled(h),
            shm_size: h.shm_size.max(0) as u64,
            sysctls: h.sysctls.clone().unwrap_or_default().into_iter().collect(),
            rlimits,
            init,
        };
        let spec_path = ctr.dir.join("runtime.json");
        write_atomic(&spec_path, &serde_json::to_vec(&spec).expect("json"))?;
        let procs = network::cgroup_create(&c.id, h).map_err(|e| ApiError(500, e.to_string()))?;
        let id = c.id.clone();
        let net = c.net;
        let bridge = nr.bridge.clone();
        let spawned = runtime::spawn(paths::PIOS_BIN, "container-init", &spec_path, &procs, c.request.tty, c.request.open_stdin, &mut |pid| {
            if net == NetMode::Bridge {
                network::attach(&id, pid, &bridge)
            } else {
                Ok(())
            }
        });
        drop(procs);
        let spawned = match spawned {
            Ok(s) => s,
            Err(e) => {
                network::detach(&id);
                network::cgroup_remove(&id);
                let msg = e.to_string();
                c.state.error = msg.clone();
                c.state.exit_code = if msg.contains("not found") || msg.contains("no such file") { 127 } else { 126 };
                c.state.status = if c.state.started_at > 0 { Status::Exited } else { Status::Created };
                ctr.save(&c);
                // dockerd's wording, which the CLI maps to exit codes 126/127.
                return Err(ApiError(400, format!("failed to create task for container: OCI runtime create failed: {msg}")));
            }
        };
        if let (Some(ip), false) = (ip, ports.is_empty()) {
            if let Err(e) = network::publish(&ports, ip, &nr.bridge) {
                warn!("engine", "{}: {e}", &id[..12]);
            }
        }
        c.ports = ports;
        c.ip = ip;
        c.manually_stopped = false;
        c.state = State { status: Status::Running, pid: spawned.pid, started_at: now_ns(), ..State::default() };
        ctr.save(&c);
        info!("engine", "started {} ({}) pid {}", &id[..12], c.name, spawned.pid);
        let runtime::Spawned { child, io, .. } = spawned;
        {
            let mut live = ctr.live();
            live.stdin = io.stdin;
            live.pty = io.pty.as_ref().and_then(|p| p.try_clone().ok());
        }
        ctr.cv.notify_all();
        drop(c);

        let mut outputs: Vec<(u8, File)> = Vec::new();
        if let Some(p) = io.pty {
            outputs.push((logs::STDOUT, p));
        }
        if let Some(o) = io.stdout {
            outputs.push((logs::STDOUT, o));
        }
        if let Some(e) = io.stderr {
            outputs.push((logs::STDERR, e));
        }
        let (pump_ctr, max, files) = (ctr.clone(), self.log_max, self.log_files);
        std::thread::spawn(move || {
            let mut log = LogWriter::open(&pump_ctr.log_path(), max, files).ok();
            pump(outputs, |stream, data| {
                if let Some(l) = &mut log {
                    let _ = l.write(stream, now_ns(), data);
                }
                let chunk: Chunk = (stream, Arc::new(data.to_vec()));
                pump_ctr.live().subscribers.retain(|s| s.send(chunk.clone()).is_ok());
            });
            pump_ctr.live().subscribers.clear();
        });
        let (me, wctr) = (self.clone(), ctr.clone());
        std::thread::spawn(move || me.on_exit(&wctr, child));
        Ok(true)
    }

    /// Waiter thread body: the helper exited, so did the container.
    fn on_exit(self: &Arc<Self>, ctr: &Arc<Ctr>, mut child: std::process::Child) {
        let code = match child.wait() {
            Ok(s) => s.code().unwrap_or_else(|| 128 + std::os::unix::process::ExitStatusExt::signal(&s).unwrap_or(0)),
            Err(_) => 255,
        };
        let (id, auto_remove, restart, started_at) = {
            let mut c = ctr.lock();
            let oom = network::cgroup_oom_killed(&c.id);
            if let Some(ip) = c.ip {
                network::unpublish(&c.ports, ip, &self.net_ref(&c).bridge);
            }
            network::detach(&c.id);
            network::cgroup_remove(&c.id);
            c.state.status = Status::Exited;
            c.state.exit_code = code;
            c.state.pid = 0;
            c.state.finished_at = now_ns();
            c.state.oom_killed = oom;
            c.ip = None;
            let restart = !self.shutting_down.load(Ordering::SeqCst) && c.should_restart(code, false);
            if restart {
                c.state.status = Status::Restarting;
            }
            ctr.save(&c);
            {
                let mut live = ctr.live();
                live.stdin = None;
                live.pty = None;
            }
            ctr.exits.fetch_add(1, Ordering::SeqCst);
            ctr.cv.notify_all();
            info!("engine", "{} ({}) exited with {code}", c.short_id(), c.name);
            (c.id.clone(), c.request.host_config.auto_remove, restart, c.state.started_at)
        };
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        if auto_remove {
            if let Err(e) = self.remove(ctr, true, true) {
                warn!("engine", "auto-remove {}: {}", &id[..12], e.1);
            }
            return;
        }
        if !restart {
            return;
        }
        // Docker's backoff: doubling from 100 ms, reset after 10 s of uptime.
        let ran = (now_ns() - started_at) / 1_000_000_000;
        let count = if ran >= 10 { 0 } else { ctr.lock().restart_count };
        let delay = Duration::from_millis((100u64 << count.min(10)).min(60_000));
        std::thread::sleep(delay);
        {
            let mut c = ctr.lock();
            if c.state.status != Status::Restarting || ctr.removed.load(Ordering::SeqCst) || self.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            c.restart_count += 1;
            c.state.status = Status::Exited;
        }
        if let Err(e) = self.start(ctr) {
            error!("engine", "restart {}: {}", &id[..12], e.1);
        }
    }

    // ------------------------------------------------------- stop/kill/remove

    fn signal(&self, ctr: &Ctr, sig: i32) -> ApiResult<()> {
        let c = ctr.lock();
        if c.state.status != Status::Running || c.state.pid <= 0 {
            return Err(ApiError(409, format!("Container {} is not running", c.id)));
        }
        // SAFETY: plain kill(2) on the container's init.
        if unsafe { libc::kill(c.state.pid, sig) } < 0 {
            return Err(ApiError(500, format!("kill: {}", io::Error::last_os_error())));
        }
        Ok(())
    }

    /// Ok(false) when it was not running.
    fn stop(&self, ctr: &Ctr, timeout: Option<i64>, signal: Option<i32>) -> ApiResult<bool> {
        let (sig, grace) = {
            let mut c = ctr.lock();
            if c.state.status == Status::Restarting {
                c.manually_stopped = true;
                c.state.status = Status::Exited;
                ctr.save(&c);
                ctr.cv.notify_all();
                return Ok(true);
            }
            if c.state.status != Status::Running {
                return Ok(false);
            }
            c.manually_stopped = true;
            ctr.save(&c);
            let sig = signal.or_else(|| c.request.stop_signal.as_deref().and_then(signal_number)).unwrap_or(libc::SIGTERM);
            let grace = timeout.or(c.request.stop_timeout).unwrap_or(10).max(0);
            (sig, grace)
        };
        let before = ctr.exits.load(Ordering::SeqCst);
        let _ = self.signal(ctr, sig);
        let exited = |_: &Container, x: &Ctr| x.exits.load(Ordering::SeqCst) != before;
        if !ctr.wait_until(Duration::from_secs(grace as u64), exited) {
            network::cgroup_kill(&ctr.lock().id);
            let _ = self.signal(ctr, libc::SIGKILL);
            ctr.wait_until(Duration::from_secs(10), exited);
        }
        Ok(true)
    }

    fn remove(&self, ctr: &Arc<Ctr>, force: bool, volumes: bool) -> ApiResult<()> {
        if ctr.removed.load(Ordering::SeqCst) {
            return Ok(());
        }
        {
            let c = ctr.lock();
            if c.running() && !force {
                return Err(ApiError(409, format!(
                    "cannot remove container \"/{}\": container is running: stop the container before removing or force remove",
                    c.name
                )));
            }
        }
        self.stop(ctr, Some(0), Some(libc::SIGKILL))?;
        let mut c = ctr.lock();
        if ctr.removed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        c.state.status = Status::Removing;
        let _ = fs::remove_dir_all(&ctr.dir);
        if volumes {
            for m in &c.mounts {
                // Anonymous volumes are named by a random 64 hex id.
                if m.kind == MountKind::Volume && m.source.len() == 64 && m.source.bytes().all(|b| b.is_ascii_hexdigit()) {
                    let _ = fs::remove_dir_all(self.volumes_dir().join(&m.source));
                }
            }
        }
        self.containers.lock().unwrap().remove(&c.id);
        ctr.cv.notify_all();
        info!("engine", "removed {} ({})", c.short_id(), c.name);
        Ok(())
    }

    // ------------------------------------------------------------- networks

    fn networks_file(&self) -> PathBuf {
        self.root.join("networks.json")
    }

    fn save_networks(&self, nets: &[Network]) {
        if let Err(e) = write_atomic(&self.networks_file(), &serde_json::to_vec_pretty(nets).expect("json")) {
            warn!("engine", "save networks: {e}");
        }
    }

    /// A user-defined network by name, id or unique id prefix.
    fn find_network(&self, key: &str) -> Option<Network> {
        if key.is_empty() || matches!(key, "bridge" | "host" | "none" | "default") {
            return None;
        }
        let nets = self.networks.lock().unwrap();
        if let Some(n) = nets.iter().find(|n| n.name == key || n.id == key) {
            return Some(n.clone());
        }
        let m: Vec<&Network> = nets.iter().filter(|n| n.id.starts_with(key)).collect();
        (m.len() == 1).then(|| m[0].clone())
    }

    fn net_ref(&self, c: &Container) -> NetRef {
        if let Some(n) = c.network.as_deref().and_then(|id| self.find_network(id)) {
            return NetRef { name: n.name, id: n.id, gateway: n.gateway, subnet: n.subnet, prefix: n.prefix, bridge: n.bridge };
        }
        let name = c.net.name().to_string();
        NetRef { id: name.clone(), name, gateway: GATEWAY, subnet: SUBNET, prefix: PREFIX, bridge: network::BRIDGE.into() }
    }

    /// Bring up a network's bridge and its DNS server.
    fn activate_network(self: &Arc<Self>, n: &Network) -> Result<()> {
        network::setup_bridge(&n.bridge, n.gateway, n.subnet, n.prefix)?;
        let (me, id, name) = (self.clone(), n.id.clone(), n.name.clone());
        let lookup: Arc<dyn Fn(&str) -> Vec<Ipv4Addr> + Send + Sync> = Arc::new(move |q: &str| {
            me.all()
                .iter()
                .filter_map(|c| {
                    let c = c.lock();
                    let ip = c.ip.filter(|_| c.network.as_deref() == Some(id.as_str()) && c.running())?;
                    let mut names = c.aliases.clone();
                    names.extend([c.name.clone(), c.hostname.clone(), c.short_id().to_string()]);
                    crate::dns::matches(q, &name, &names).then_some(ip)
                })
                .collect()
        });
        let (gw, name) = (n.gateway, n.name.clone());
        std::thread::spawn(move || crate::dns::server::serve(gw, name, lookup));
        Ok(())
    }

    fn create_network(self: &Arc<Self>, req: &NetworkCreate) -> ApiResult<Network> {
        if req.name.is_empty() || !valid_volume_name(&req.name) {
            return Err(bad_request(format!("invalid network name {:?}", req.name)));
        }
        if !req.driver.is_empty() && req.driver != "bridge" {
            return Err(bad_request(format!("network driver {:?} is not supported (bridge only)", req.driver)));
        }
        let mut nets = self.networks.lock().unwrap();
        if matches!(req.name.as_str(), "bridge" | "host" | "none") || nets.iter().any(|n| n.name == req.name) {
            return Err(ApiError(409, format!("network with name {} already exists", req.name)));
        }
        let used: Vec<Ipv4Addr> = nets.iter().map(|n| n.subnet).collect();
        let subnet = allocate_subnet(&used).ok_or_else(|| ApiError(500, "no free subnet for a new network".into()))?;
        let id = random_id();
        let o = subnet.octets();
        let n = Network {
            bridge: format!("br-{}", &id[..12]),
            id,
            name: req.name.clone(),
            subnet,
            prefix: 16,
            gateway: Ipv4Addr::new(o[0], o[1], 0, 1),
            labels: req.labels.clone().unwrap_or_default(),
            created: now_ns(),
            internal: req.internal,
        };
        nets.push(n.clone());
        self.save_networks(&nets);
        drop(nets);
        self.activate_network(&n)?;
        info!("engine", "network {} created ({}/16 on {})", n.name, n.subnet, n.bridge);
        Ok(n)
    }

    fn remove_network(&self, key: &str) -> ApiResult<()> {
        let n = self.find_network(key).ok_or_else(|| {
            if matches!(key, "bridge" | "host" | "none") {
                ApiError(403, format!("{key} is a pre-defined network and cannot be removed"))
            } else {
                ApiError(404, format!("network {key} not found"))
            }
        })?;
        let users: Vec<String> = self.all().iter().map(|c| c.lock()).filter(|c| c.network.as_deref() == Some(n.id.as_str())).map(|c| c.name.clone()).collect();
        if !users.is_empty() {
            return Err(ApiError(403, format!("error while removing network: network {} has active endpoints ({})", n.name, users.join(", "))));
        }
        let mut nets = self.networks.lock().unwrap();
        nets.retain(|x| x.id != n.id);
        self.save_networks(&nets);
        drop(nets);
        // Deleting the bridge removes the gateway address, which ends the DNS thread.
        network::teardown_bridge(&n.bridge, n.subnet, n.prefix);
        info!("engine", "network {} removed", n.name);
        Ok(())
    }

    fn network_json(&self, r: &NetRef, labels: &BTreeMap<String, String>, created: i64, internal: bool) -> Value {
        let mut containers = serde_json::Map::new();
        for c in self.all() {
            let c = c.lock();
            if self.net_ref(&c).id != r.id {
                continue;
            }
            if let Some(ip) = c.ip {
                containers.insert(c.id.clone(), json!({"Name": c.name, "EndpointID": c.id, "MacAddress": mac_for(ip), "IPv4Address": format!("{ip}/{}", r.prefix), "IPv6Address": ""}));
            }
        }
        let builtin_host = matches!(r.id.as_str(), "host" | "none");
        let ipam = if builtin_host {
            json!({"Driver": "default", "Options": null, "Config": []})
        } else {
            json!({"Driver": "default", "Options": null, "Config": [{"Subnet": format!("{}/{}", r.subnet, r.prefix), "Gateway": r.gateway.to_string()}]})
        };
        json!({
            "Name": r.name, "Id": r.id, "Created": rfc3339(created), "Scope": "local",
            "Driver": if builtin_host { r.id.as_str() } else { "bridge" }, "EnableIPv4": true, "EnableIPv6": false, "IPAM": ipam,
            "Internal": internal, "Attachable": false, "Ingress": false, "ConfigFrom": {"Network": ""}, "ConfigOnly": false,
            "Containers": containers, "Options": {"com.docker.network.bridge.name": r.bridge}, "Labels": labels,
        })
    }

    fn all_networks_json(&self) -> Vec<(Value, BTreeMap<String, String>, String, String)> {
        let mut out = Vec::new();
        for name in ["bridge", "host", "none"] {
            let r = NetRef { name: name.into(), id: name.into(), gateway: GATEWAY, subnet: SUBNET, prefix: PREFIX, bridge: if name == "bridge" { network::BRIDGE.into() } else { String::new() } };
            out.push((self.network_json(&r, &BTreeMap::new(), 0, false), BTreeMap::new(), name.to_string(), name.to_string()));
        }
        // Copy first so the lock is released before the loop: network_json() takes
        // it again via find_network(), and a `for` over the guard temporary would
        // keep it held for the whole loop (self-deadlock).
        let user_nets = self.networks.lock().unwrap().clone();
        for n in user_nets {
            let r = NetRef { name: n.name.clone(), id: n.id.clone(), gateway: n.gateway, subnet: n.subnet, prefix: n.prefix, bridge: n.bridge.clone() };
            out.push((self.network_json(&r, &n.labels, n.created, n.internal), n.labels.clone(), n.name, n.id));
        }
        out
    }

    // -------------------------------------------------------------- routing

    fn route(self: &Arc<Self>, req: &Request, w: &mut UnixStream) -> ApiResult<Option<(u16, Value)>> {
        let seg = req.segments();
        let m = req.method.as_str();
        let ok = |v: Value| Ok(Some((200, v)));
        match (m, seg.as_slice()) {
            ("GET" | "HEAD", ["_ping"]) => Ok(None),
            ("GET", ["version"]) => ok(self.version()),
            ("GET", ["info"]) => ok(self.info()),
            ("GET", ["containers", "json"]) => {
                let f = Filters::parse(req.query("filters")).map_err(bad_request)?;
                // Explicit status filters imply all containers, as in dockerd.
                let all = req.flag("all") || !f.get("status").is_empty();
                let mut list: Vec<(i64, Value)> = self
                    .all()
                    .iter()
                    .map(|c| c.lock())
                    .filter(|c| all || c.running())
                    .filter(|c| f.labels_match(&c.request.labels.clone().unwrap_or_default()))
                    .filter(|c| f.any("name", |n| c.name.contains(n.trim_start_matches('/'))))
                    .filter(|c| f.any("id", |i| c.id.starts_with(i)))
                    .filter(|c| f.any("status", |s| s == c.state.status.name()))
                    .filter(|c| f.any("network", |n| { let r = self.net_ref(c); r.name == n || r.id == n }))
                    .map(|c| (c.created, c.summary(&self.net_ref(&c).info(), &self.volumes_dir().display().to_string())))
                    .collect();
                list.sort_by_key(|(t, _)| -t);
                ok(Value::Array(list.into_iter().map(|(_, v)| v).collect()))
            }
            ("POST", ["containers", "create"]) => self.create(req).map(Some),
            ("POST", ["containers", "prune"]) => {
                let mut deleted = Vec::new();
                for c in self.all() {
                    let id = c.lock().id.clone();
                    if !c.lock().running() && self.remove(&c, false, false).is_ok() {
                        deleted.push(id);
                    }
                }
                ok(json!({"ContainersDeleted": deleted, "SpaceReclaimed": 0}))
            }
            ("GET", ["containers", id, "json"]) => {
                let c = self.find(id)?;
                let c = c.lock();
                let dir = self.containers_dir().join(&c.id);
                ok(c.inspect(&self.net_ref(&c).info(), &self.volumes_dir().display().to_string(), &dir.display().to_string()))
            }
            ("POST", ["containers", id, "start"]) => {
                let c = self.find(id)?;
                match self.start(&c) {
                    Ok(started) => Ok(Some((if started { 204 } else { 304 }, Value::Null))),
                    Err(e) => {
                        // Like dockerd: a --rm container that cannot start is removed
                        // (the client may be waiting for condition=removed).
                        if c.lock().request.host_config.auto_remove {
                            let _ = self.remove(&c, true, true);
                        }
                        Err(e)
                    }
                }
            }
            ("POST", ["containers", id, "stop"]) => {
                let c = self.find(id)?;
                let t = req.query("t").or(req.query("timeout")).and_then(|t| t.parse().ok());
                let sig = req.query("signal").and_then(signal_number);
                Ok(Some((if self.stop(&c, t, sig)? { 204 } else { 304 }, Value::Null)))
            }
            ("POST", ["containers", id, "restart"]) => {
                let c = self.find(id)?;
                let t = req.query("t").and_then(|t| t.parse().ok());
                self.stop(&c, t, None)?;
                self.start(&c)?;
                Ok(Some((204, Value::Null)))
            }
            ("POST", ["containers", id, "kill"]) => {
                let c = self.find(id)?;
                let sig = match req.query("signal") {
                    Some(s) => signal_number(s).ok_or_else(|| bad_request(format!("Invalid signal: {s}")))?,
                    None => libc::SIGKILL,
                };
                if sig == libc::SIGKILL {
                    let mut g = c.lock();
                    g.manually_stopped = true;
                    c.save(&g);
                }
                self.signal(&c, sig)?;
                Ok(Some((204, Value::Null)))
            }
            ("POST", ["containers", id, "wait"]) => {
                let c = self.find(id)?;
                let cond = req.query("condition").unwrap_or("not-running").to_string();
                let start = c.exits.load(Ordering::SeqCst);
                // The client waits for the headers before it starts the
                // container (docker run), so send them now and the body later.
                http::respond_chunked(w, 200, "application/json")?;
                c.wait_until(Duration::from_secs(365 * 86_400), |g, x| match cond.as_str() {
                    "next-exit" => x.exits.load(Ordering::SeqCst) != start,
                    "removed" => x.removed.load(Ordering::SeqCst),
                    _ => !g.running(),
                });
                let body = {
                    let g = c.lock();
                    let err = if g.state.error.is_empty() { Value::Null } else { json!({"Message": g.state.error}) };
                    format!("{}\n", json!({"StatusCode": g.state.exit_code, "Error": err}))
                };
                write_chunk(w, body.as_bytes())?;
                w.write_all(b"0\r\n\r\n")?;
                Err(ApiError(0, String::new()))
            }
            ("POST", ["containers", id, "resize"]) => {
                let c = self.find(id)?;
                let (h, wd) = dims(req);
                if let Some(p) = &c.live().pty {
                    let _ = runtime::resize(p, h, wd);
                }
                Ok(Some((200, Value::Null)))
            }
            ("POST", ["containers", id, "rename"]) => {
                let c = self.find(id)?;
                let name = req.query("name").unwrap_or_default().trim_start_matches('/').to_string();
                if !valid_container_name(&name) {
                    return Err(bad_request(format!("Invalid container name ({name})")));
                }
                if self.all().iter().any(|o| !Arc::ptr_eq(o, &c) && o.lock().name == name) {
                    return Err(ApiError(409, format!("Conflict. The container name \"/{name}\" is already in use")));
                }
                let mut g = c.lock();
                g.name = name;
                c.save(&g);
                Ok(Some((204, Value::Null)))
            }
            ("DELETE", ["containers", id]) => {
                let c = self.find(id)?;
                self.remove(&c, req.flag("force"), req.flag("v"))?;
                Ok(Some((204, Value::Null)))
            }
            ("GET", ["containers", id, "logs"]) => {
                let c = self.find(id)?;
                self.logs(&c, req, w)?;
                Err(ApiError(0, String::new()))
            }
            ("POST", ["containers", id, "exec"]) => self.exec_create(id, req).map(Some),
            ("GET", ["exec", id, "json"]) => ok(self.exec_inspect(id)?),
            ("POST", ["exec", id, "resize"]) => {
                let e = self.execs.lock().unwrap().get(*id).cloned().ok_or_else(|| ApiError(404, format!("No such exec instance: {id}")))?;
                let (h, wd) = dims(req);
                if let Some(p) = &*e.pty.lock().unwrap() {
                    let _ = runtime::resize(p, h, wd);
                }
                Ok(Some((200, Value::Null)))
            }
            ("GET", ["images", "json"]) => {
                let mut list: Vec<Value> = self.store.image_ids().iter().filter_map(|i| self.store.load(i)).map(|i| self.store.summary(&i)).collect();
                list.sort_by_key(|v| -v["Created"].as_i64().unwrap_or(0));
                ok(Value::Array(list))
            }
            ("POST", ["images", "create"]) => {
                self.pull(req, w)?;
                Err(ApiError(0, String::new()))
            }
            ("GET", ["images", rest @ .., "json"]) if !rest.is_empty() => {
                let name = rest.join("/");
                let img = self.store.resolve(&name).ok_or_else(|| ApiError(404, format!("No such image: {name}")))?;
                ok(self.store.inspect(&img))
            }
            ("DELETE", ["images", rest @ ..]) if !rest.is_empty() => {
                let name = rest.join("/");
                let in_use = |id: &str| self.all().iter().any(|c| c.lock().image_id == id);
                let r = self.store.remove(&name, req.flag("force"), &in_use).map_err(|e| {
                    let m = e.to_string();
                    ApiError(if m.starts_with("No such image") { 404 } else { 409 }, m)
                })?;
                ok(Value::Array(r))
            }
            ("GET", ["networks"]) => {
                let f = Filters::parse(req.query("filters")).map_err(bad_request)?;
                let list: Vec<Value> = self
                    .all_networks_json()
                    .into_iter()
                    .filter(|(_, labels, _, _)| f.labels_match(labels))
                    .filter(|(_, _, name, _)| f.any("name", |n| name.contains(n)))
                    .filter(|(_, _, _, id)| f.any("id", |i| id.starts_with(i)))
                    .filter(|(_, _, name, _)| f.any("type", |t| (t == "builtin") == matches!(name.as_str(), "bridge" | "host" | "none")))
                    .map(|(v, ..)| v)
                    .collect();
                ok(Value::Array(list))
            }
            ("GET", ["networks", key]) => self
                .all_networks_json()
                .into_iter()
                .find(|(_, _, name, id)| name == key || id == key || (key.len() >= 6 && id.starts_with(key)))
                .map(|(v, ..)| Some((200, v)))
                .ok_or_else(|| ApiError(404, format!("network {key} not found"))),
            ("POST", ["networks", "create"]) => {
                let body: NetworkCreate = serde_json::from_slice(&req.body).map_err(|e| bad_request(format!("invalid network request: {e}")))?;
                let n = self.create_network(&body)?;
                Ok(Some((201, json!({"Id": n.id, "Warning": ""}))))
            }
            ("DELETE", ["networks", key]) => {
                self.remove_network(key)?;
                Ok(Some((204, Value::Null)))
            }
            ("POST", ["networks", "prune"]) => {
                let mut deleted = Vec::new();
                for n in self.networks.lock().unwrap().clone() {
                    if self.remove_network(&n.id).is_ok() {
                        deleted.push(n.name);
                    }
                }
                ok(json!({"NetworksDeleted": deleted}))
            }
            ("POST", ["networks", key, "connect" | "disconnect"]) => {
                // One network per container: accept only what is already true.
                #[derive(Deserialize, Default)]
                #[serde(rename_all = "PascalCase", default)]
                struct C {
                    container: String,
                }
                let body: C = serde_json::from_slice(&req.body).unwrap_or_default();
                let c = self.find(&body.container)?;
                let r = self.net_ref(&c.lock());
                let connect = seg[2] == "connect";
                if connect && (r.name == *key || r.id == *key) {
                    return Ok(Some((200, Value::Null)));
                }
                Err(ApiError(501, "the PiOS native engine attaches a container to exactly one network (set it at create time)".into()))
            }
            ("GET", ["volumes"]) => {
                let f = Filters::parse(req.query("filters")).map_err(bad_request)?;
                let vols: Vec<Value> = fs::read_dir(self.volumes_dir())
                    .map(|rd| {
                        rd.flatten()
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .filter(|n| f.any("name", |x| n.contains(x)))
                            .filter(|n| f.labels_match(&self.volume_labels(n)))
                            .map(|n| self.volume_json(&n))
                            .collect()
                    })
                    .unwrap_or_default();
                ok(json!({"Volumes": vols, "Warnings": []}))
            }
            ("POST", ["volumes", "create"]) => {
                #[derive(Deserialize, Default)]
                #[serde(rename_all = "PascalCase", default)]
                struct V {
                    name: String,
                    labels: Option<BTreeMap<String, String>>,
                }
                let v: V = serde_json::from_slice(&req.body).unwrap_or_default();
                let name = if v.name.is_empty() { random_id() } else { v.name };
                if !valid_volume_name(&name) {
                    return Err(bad_request(format!("invalid volume name {name:?}")));
                }
                let fresh = !self.volumes_dir().join(&name).exists();
                fs::create_dir_all(self.volume_path(&name))?;
                if fresh {
                    if let Some(l) = v.labels.filter(|l| !l.is_empty()) {
                        write_atomic(&self.volumes_dir().join(&name).join("labels.json"), &serde_json::to_vec(&l).expect("json"))?;
                    }
                }
                Ok(Some((201, self.volume_json(&name))))
            }
            ("GET", ["volumes", name]) => {
                if !self.volumes_dir().join(name).is_dir() {
                    return Err(ApiError(404, format!("get {name}: no such volume")));
                }
                ok(self.volume_json(name))
            }
            ("DELETE", ["volumes", name]) => {
                let users: Vec<String> = self
                    .all()
                    .iter()
                    .map(|c| c.lock())
                    .filter(|c| c.mounts.iter().any(|m| m.kind == MountKind::Volume && m.source == *name))
                    .map(|c| c.id.clone())
                    .collect();
                if !users.is_empty() {
                    return Err(ApiError(409, format!("remove {name}: volume is in use - [{}]", users.join(", "))));
                }
                if !valid_volume_name(name) || !self.volumes_dir().join(name).is_dir() {
                    return Err(ApiError(404, format!("get {name}: no such volume")));
                }
                fs::remove_dir_all(self.volumes_dir().join(name))?;
                Ok(Some((204, Value::Null)))
            }
            _ => Err(ApiError(
                if m == "GET" || m == "POST" || m == "DELETE" { 501 } else { 405 },
                format!("{m} {} is not implemented by the PiOS native engine", req.path),
            )),
        }
    }

    fn volume_json(&self, name: &str) -> Value {
        let created = fs::metadata(self.volumes_dir().join(name))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i64);
        json!({
            "Name": name, "Driver": "local", "Mountpoint": self.volume_path(name).display().to_string(),
            "CreatedAt": rfc3339(created), "Labels": self.volume_labels(name), "Scope": "local", "Options": {},
        })
    }

    fn volume_labels(&self, name: &str) -> BTreeMap<String, String> {
        fs::read(self.volumes_dir().join(name).join("labels.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn version(&self) -> Value {
        let kernel = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string();
        let v = format!("pios-native {}", env!("CARGO_PKG_VERSION"));
        json!({
            "Platform": {"Name": "PiOS native engine"},
            "Components": [{"Name": "Engine", "Version": v, "Details": {"ApiVersion": "1.47", "MinAPIVersion": "1.24", "Os": "linux", "Arch": "arm", "KernelVersion": kernel}}],
            "Version": v, "ApiVersion": "1.47", "MinAPIVersion": "1.24", "GitCommit": "", "GoVersion": "none (Rust)",
            "Os": "linux", "Arch": "arm", "KernelVersion": kernel, "BuildTime": "",
        })
    }

    fn info(&self) -> Value {
        let all = self.all();
        let running = all.iter().filter(|c| c.lock().running()).count();
        let os = fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|t| t.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())))
            .unwrap_or_else(|| "PiOS".into());
        json!({
            "ID": "pios-native", "Containers": all.len(), "ContainersRunning": running, "ContainersPaused": 0,
            "ContainersStopped": all.len() - running, "Images": self.store.image_ids().len(), "Driver": "overlay2",
            "DriverStatus": [["Backing Filesystem", "extfs"]], "MemoryLimit": true, "SwapLimit": true, "CpuCfsPeriod": true,
            "CpuCfsQuota": true, "PidsLimit": true, "IPv4Forwarding": true, "Debug": false, "NFd": 0, "NGoroutines": 0,
            "SystemTime": rfc3339(now_ns()), "LoggingDriver": "local", "CgroupDriver": "cgroupfs", "CgroupVersion": "2",
            "KernelVersion": fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim(),
            "OperatingSystem": os, "OSType": "linux", "Architecture": "armv7l",
            "NCPU": std::thread::available_parallelism().map_or(1, |n| n.get()), "MemTotal": pios_core::sys::total_ram(),
            "IndexServerAddress": "https://index.docker.io/v1/",
            "RegistryConfig": {"IndexConfigs": {}, "InsecureRegistryCIDRs": ["127.0.0.0/8"], "Mirrors": []},
            "DockerRootDir": self.root.display().to_string(), "Name": self.hostname,
            "ServerVersion": format!("pios-native {}", env!("CARGO_PKG_VERSION")),
            "Runtimes": {"pios": {"path": paths::PIOS_BIN}}, "DefaultRuntime": "pios",
            "Swarm": {"LocalNodeState": "inactive", "NodeID": "", "NodeAddr": "", "ControlAvailable": false, "Error": "", "RemoteManagers": null},
            "SecurityOptions": ["name=seccomp,profile=builtin", "name=cgroupns"], "Labels": [], "ExperimentalBuild": false, "LiveRestoreEnabled": false,
            "Warnings": [],
        })
    }

    // ----------------------------------------------------------------- pull

    fn pull(&self, req: &Request, w: &mut UnixStream) -> ApiResult<()> {
        let from = req.query("fromImage").unwrap_or_default();
        if from.is_empty() {
            return Err(bad_request("fromImage is required (import is not supported)"));
        }
        let name = match req.query("tag").filter(|t| !t.is_empty()) {
            Some(t) if t.starts_with("sha256:") => format!("{from}@{t}"),
            Some(t) => format!("{from}:{t}"),
            None => from.to_string(),
        };
        let r = Reference::parse(&name).map_err(bad_request)?;
        if let Some(p) = req.query("platform").filter(|p| !p.is_empty() && !p.starts_with("linux/arm")) {
            return Err(bad_request(format!("platform {p} cannot run on this board (linux/arm/v7)")));
        }
        let client = self.client.as_ref().ok_or_else(|| ApiError(500, "registry client unavailable (CA certificates missing)".into()))?;
        let auth = req.header("x-registry-auth").and_then(Auth::from_header);
        http::respond_chunked(w, 200, "application/json")?;
        let writer = Mutex::new(w.try_clone()?);
        let mut emit = |v: Value| {
            let mut line = v.to_string();
            line.push_str("\r\n");
            let _ = write_chunk(&mut writer.lock().unwrap(), line.as_bytes());
        };
        if let Err(e) = registry::pull(client, &self.store, &r, auth, self.cfg.max_concurrent_downloads as usize, &mut emit) {
            warn!("engine", "pull {r}: {e}");
            emit(json!({"errorDetail": {"message": e.to_string()}, "error": e.to_string()}));
        }
        let _ = writer.lock().unwrap().write_all(b"0\r\n\r\n");
        Ok(())
    }

    // ----------------------------------------------------------------- logs

    fn logs(&self, ctr: &Arc<Ctr>, req: &Request, w: &mut UnixStream) -> ApiResult<()> {
        let (want_out, want_err) = (req.flag("stdout"), req.flag("stderr"));
        let timestamps = req.flag("timestamps");
        let since = req.query("since").and_then(|s| s.parse::<f64>().ok()).map_or(0, |s| (s * 1e9) as i64);
        let until = req.query("until").and_then(|s| s.parse::<f64>().ok()).filter(|u| *u > 0.0).map_or(i64::MAX, |s| (s * 1e9) as i64);
        let tail = req.query("tail").and_then(|t| t.parse::<usize>().ok());
        let tty = ctr.lock().tty();
        let follow = req.flag("follow") && ctr.lock().running();
        // Subscribe before reading the file so nothing falls between.
        let rx = follow.then(|| {
            let (tx, rx) = mpsc::channel();
            ctr.live().subscribers.push(tx);
            rx
        });
        let ct = if tty { "application/vnd.docker.raw-stream" } else { "application/vnd.docker.multiplexed-stream" };
        http::respond_chunked(w, 200, ct)?;
        let frame = |stream: u8, time: i64, data: &[u8]| -> Vec<u8> {
            let mut payload = Vec::new();
            if timestamps {
                payload.extend_from_slice(rfc3339(time).as_bytes());
                payload.push(b' ');
            }
            payload.extend_from_slice(data);
            if tty {
                payload
            } else {
                mux_frame(stream, &payload)
            }
        };
        let wanted = |s: u8| (s == logs::STDOUT && want_out) || (s == logs::STDERR && want_err);
        let mut lines: Vec<(u8, i64, Vec<u8>)> = Vec::new();
        for r in logs::read_all(&ctr.log_path(), self.log_files) {
            if !wanted(r.stream) || r.time_ns < since || r.time_ns > until {
                continue;
            }
            for l in r.data.split_inclusive(|b| *b == b'\n') {
                lines.push((r.stream, r.time_ns, l.to_vec()));
            }
        }
        let skip = tail.map_or(0, |t| lines.len().saturating_sub(t));
        for (s, t, l) in &lines[skip..] {
            write_chunk(w, &frame(*s, *t, l))?;
        }
        if let Some(rx) = rx {
            for (s, data) in rx {
                if wanted(s) && write_chunk(w, &frame(s, now_ns(), &data)).is_err() {
                    break;
                }
            }
        }
        w.write_all(b"0\r\n\r\n")?;
        Ok(())
    }

    // --------------------------------------------------------------- attach

    fn attach(&self, ctr: &Arc<Ctr>, req: &Request, reader: BufReader<UnixStream>, mut w: UnixStream) -> io::Result<()> {
        let (tty, open_stdin, stdin_once) = {
            let c = ctr.lock();
            (c.tty(), c.request.open_stdin, c.request.stdin_once)
        };
        upgrade(req, &mut w, tty)?;
        let (want_out, want_err) = (req.flag("stdout"), req.flag("stderr"));
        let wanted = |s: u8| (s == logs::STDOUT && want_out) || (s == logs::STDERR && want_err);
        let send = |w: &mut UnixStream, s: u8, d: &[u8]| -> io::Result<()> {
            if tty {
                w.write_all(d)
            } else {
                w.write_all(&mux_frame(s, d))
            }
        };
        if req.flag("logs") {
            for r in logs::read_all(&ctr.log_path(), self.log_files) {
                if wanted(r.stream) {
                    send(&mut w, r.stream, &r.data)?;
                }
            }
        }
        if !req.flag("stream") {
            let _ = w.shutdown(Shutdown::Both);
            return Ok(());
        }
        let rx = {
            let c = ctr.lock();
            let finished = matches!(c.state.status, Status::Exited | Status::Dead) && !c.request.host_config.auto_remove && c.state.started_at > 0;
            drop(c);
            if finished {
                None
            } else {
                let (tx, rx) = mpsc::channel();
                ctr.live().subscribers.push(tx);
                Some(rx)
            }
        };
        if req.flag("stdin") && open_stdin {
            let ctr = ctr.clone();
            std::thread::spawn(move || copy_stdin(reader, &ctr, stdin_once));
        }
        if let Some(rx) = rx {
            for (s, data) in rx {
                if wanted(s) && send(&mut w, s, &data).is_err() {
                    break;
                }
            }
        }
        let _ = w.shutdown(Shutdown::Both);
        Ok(())
    }

    // ----------------------------------------------------------------- exec

    fn exec_create(&self, id: &str, req: &Request) -> ApiResult<(u16, Value)> {
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "PascalCase", default)]
        struct E {
            attach_stdin: bool,
            attach_stdout: bool,
            attach_stderr: bool,
            tty: bool,
            cmd: Vec<String>,
            env: Option<Vec<String>>,
            working_dir: String,
            user: String,
            privileged: bool,
        }
        let ctr = self.find(id)?;
        let e: E = serde_json::from_slice(&req.body).map_err(|e| bad_request(format!("invalid exec request: {e}")))?;
        let c = ctr.lock();
        if c.state.status != Status::Running {
            return Err(ApiError(409, format!("container {} is not running", c.id)));
        }
        if e.cmd.is_empty() {
            return Err(bad_request("No exec command specified"));
        }
        let exec_id = random_id();
        let rec = Arc::new(ExecRec {
            id: exec_id.clone(),
            container: c.id.clone(),
            cmd: e.cmd,
            env: e.env.unwrap_or_default(),
            user: e.user,
            cwd: e.working_dir,
            tty: e.tty,
            privileged: e.privileged,
            attach_stdin: e.attach_stdin,
            attach_stdout: e.attach_stdout,
            attach_stderr: e.attach_stderr,
            running: AtomicBool::new(false),
            pid: Mutex::new(0),
            exit_code: Mutex::new(None),
            pty: Mutex::new(None),
        });
        let mut execs = self.execs.lock().unwrap();
        // Forget finished execs of containers that are gone.
        let alive: Vec<String> = self.containers.lock().unwrap().keys().cloned().collect();
        execs.retain(|_, x| alive.contains(&x.container));
        execs.insert(exec_id.clone(), rec);
        Ok((201, json!({"Id": exec_id})))
    }

    fn exec_inspect(&self, id: &str) -> ApiResult {
        let e = self.execs.lock().unwrap().get(id).cloned().ok_or_else(|| ApiError(404, format!("No such exec instance: {id}")))?;
        let code = *e.exit_code.lock().unwrap();
        Ok(json!({
            "ID": e.id, "Running": e.running.load(Ordering::SeqCst), "ExitCode": code, "ContainerID": e.container,
            "OpenStdin": e.attach_stdin, "OpenStdout": e.attach_stdout, "OpenStderr": e.attach_stderr,
            "CanRemove": false, "DetachKeys": "", "Pid": *e.pid.lock().unwrap(),
            "ProcessConfig": {"tty": e.tty, "entrypoint": e.cmd[0], "arguments": &e.cmd[1..], "privileged": e.privileged, "user": e.user},
        }))
    }

    fn exec_start(&self, id: &str, req: &Request, reader: BufReader<UnixStream>, mut w: UnixStream) -> ApiResult<()> {
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "PascalCase", default)]
        struct S {
            detach: bool,
        }
        let s: S = serde_json::from_slice(&req.body).unwrap_or_default();
        let e = self.execs.lock().unwrap().get(id).cloned().ok_or_else(|| ApiError(404, format!("No such exec instance: {id}")))?;
        if e.running.swap(true, Ordering::SeqCst) || e.exit_code.lock().unwrap().is_some() {
            return Err(ApiError(409, format!("exec {id} has already been started")));
        }
        let ctr = self.find(&e.container)?;
        let (spec, cid) = {
            let c = ctr.lock();
            if c.state.status != Status::Running {
                e.running.store(false, Ordering::SeqCst);
                return Err(ApiError(409, format!("container {} is not running", c.id)));
            }
            let mut env = c.process.env.clone();
            for kv in &e.env {
                let key = kv.split('=').next().unwrap_or(kv);
                env.retain(|x| x.split('=').next() != Some(key));
                env.push(kv.clone());
            }
            let mut host = c.request.host_config.clone();
            host.privileged |= e.privileged;
            let process = ProcessConfig {
                path: e.cmd[0].clone(),
                args: e.cmd[1..].to_vec(),
                env,
                cwd: if e.cwd.is_empty() { c.process.cwd.clone() } else { e.cwd.clone() },
                user: if e.user.is_empty() { c.process.user.clone() } else { e.user.clone() },
            };
            let spec = ExecSpec {
                pid: c.state.pid,
                process,
                tty: e.tty,
                caps: capabilities(&host).map_err(bad_request)?,
                seccomp: seccomp_enabled(&host),
                rlimits: vec![("nofile".into(), 1_048_576, 1_048_576)],
            };
            (spec, c.id.clone())
        };
        let spec_path = ctr.dir.join(format!("exec-{}.json", &e.id[..12]));
        write_atomic(&spec_path, &serde_json::to_vec(&spec).expect("json"))?;
        let procs = fs::OpenOptions::new().write(true).open(network::cgroup_path(&cid).join("cgroup.procs"))?;
        let spawned = runtime::spawn(paths::PIOS_BIN, "container-exec", &spec_path, &procs, e.tty, e.attach_stdin, &mut |_| Ok(()));
        let _ = fs::remove_file(&spec_path);
        let spawned = match spawned {
            Ok(s) => s,
            Err(err) => {
                let msg = err.to_string();
                *e.exit_code.lock().unwrap() = Some(if msg.contains("not found") || msg.contains("no such file") { 127 } else { 126 });
                e.running.store(false, Ordering::SeqCst);
                return Err(ApiError(if msg.contains("not found") { 404 } else { 500 }, format!("OCI runtime exec failed: {msg}")));
            }
        };
        *e.pid.lock().unwrap() = spawned.pid;
        let runtime::Spawned { mut child, io, .. } = spawned;
        *e.pty.lock().unwrap() = io.pty.as_ref().and_then(|p| p.try_clone().ok());
        let mut outputs: Vec<(u8, File)> = Vec::new();
        if let Some(p) = io.pty {
            outputs.push((logs::STDOUT, p));
        }
        outputs.extend(io.stdout.map(|f| (logs::STDOUT, f)));
        outputs.extend(io.stderr.map(|f| (logs::STDERR, f)));
        let finish = |child: &mut std::process::Child| {
            let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(255);
            *e.exit_code.lock().unwrap() = Some(code);
            e.running.store(false, Ordering::SeqCst);
            *e.pty.lock().unwrap() = None;
        };
        if s.detach {
            respond(&mut w, 200, "", b"", false)?;
            drop(io.stdin);
            let e2 = e.clone();
            std::thread::spawn(move || {
                pump(outputs, |_, _| {});
                let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(255);
                *e2.exit_code.lock().unwrap() = Some(code);
                e2.running.store(false, Ordering::SeqCst);
            });
            return Ok(());
        }
        upgrade(req, &mut w, e.tty)?;
        if let Some(stdin) = io.stdin {
            std::thread::spawn(move || {
                let mut reader = reader;
                let mut stdin = stdin;
                let _ = io::copy(&mut reader, &mut stdin);
            });
        }
        let tty = e.tty;
        let mut wr = w.try_clone()?;
        pump(outputs, |stream, data| {
            let _ = if tty { wr.write_all(data) } else { wr.write_all(&mux_frame(stream, data)) };
        });
        finish(&mut child);
        let _ = w.shutdown(Shutdown::Both);
        Ok(())
    }

    // ------------------------------------------------------------ connection

    fn serve(self: &Arc<Self>, stream: UnixStream) {
        let Ok(rs) = stream.try_clone() else { return };
        let mut reader = BufReader::with_capacity(32 << 10, rs);
        let mut w = stream;
        loop {
            let req = match read_request(&mut reader) {
                Ok(Some(r)) => r,
                Ok(None) => return,
                Err(e) => {
                    let _ = respond(&mut w, 400, "application/json", json!({"message": e.to_string()}).to_string().as_bytes(), true);
                    return;
                }
            };
            let seg = req.segments();
            let hijack = req.method == "POST"
                && matches!(seg.as_slice(), ["containers", _, "attach"] | ["exec", _, "start"]);
            if hijack {
                let id = seg[1].to_string();
                let r = if seg[0] == "containers" {
                    match self.find(&id) {
                        Ok(c) => self.attach(&c, &req, reader, w.try_clone().expect("clone")).map_err(ApiError::from),
                        Err(e) => Err(e),
                    }
                } else {
                    self.exec_start(&id, &req, reader, w.try_clone().expect("clone"))
                };
                if let Err(ApiError(code, msg)) = r {
                    let _ = respond(&mut w, code, "application/json", json!({"message": msg}).to_string().as_bytes(), true);
                }
                return;
            }
            let head = req.method == "HEAD";
            let result = self.route(&req, &mut w);
            let close = req.close;
            let sent = match result {
                Ok(None) => {
                    let body: &[u8] = if head { b"" } else { b"OK" };
                    let extra = "Cache-Control: no-cache, no-store, must-revalidate\r\nPragma: no-cache\r\n";
                    let r = write!(
                        w,
                        "HTTP/1.1 200 OK\r\n{API_HEADERS}{extra}Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
                        if head { 2 } else { body.len() }
                    )
                    .and_then(|_| w.write_all(body));
                    r
                }
                Ok(Some((code, v))) if code == 204 || code == 304 || v.is_null() => respond(&mut w, code, "", b"", close),
                Ok(Some((code, v))) => respond(&mut w, code, "application/json", format!("{v}\n").as_bytes(), close),
                // Streaming handlers wrote a complete chunked response.
                Err(ApiError(0, _)) => Ok(()),
                Err(ApiError(code, msg)) => {
                    if code >= 500 {
                        warn!("engine", "{} {}: {msg}", req.method, req.path);
                    }
                    respond(&mut w, code, "application/json", json!({"message": msg}).to_string().as_bytes(), close)
                }
            };
            if sent.is_err() || close {
                return;
            }
        }
    }
}

fn display_image(name: &str) -> String {
    Reference::parse(name).map_or(name.to_string(), |r| r.to_string())
}

fn dims(req: &Request) -> (u16, u16) {
    let n = |k: &str| req.query(k).and_then(|v| v.parse().ok()).unwrap_or(0);
    (n("h"), n("w"))
}


fn write_chunk(w: &mut UnixStream, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    write!(w, "{:x}\r\n", data.len())?;
    w.write_all(data)?;
    w.write_all(b"\r\n")
}

/// Hijack the connection for a raw stream (attach, exec start).
fn upgrade(req: &Request, w: &mut UnixStream, tty: bool) -> io::Result<()> {
    let ct = if tty { "application/vnd.docker.raw-stream" } else { "application/vnd.docker.multiplexed-stream" };
    if req.header("upgrade").is_some() {
        write!(w, "HTTP/1.1 101 UPGRADED\r\nContent-Type: {ct}\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n{API_HEADERS}\r\n")
    } else {
        write!(w, "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\n{API_HEADERS}\r\n")
    }
}

/// Client stdin -> container stdin. Waits for the container to start (the
/// CLI attaches before starting). StdinOnce closes it on client EOF.
fn copy_stdin(mut reader: BufReader<UnixStream>, ctr: &Arc<Ctr>, stdin_once: bool) {
    let mut buf = vec![0u8; 32 << 10];
    let handle = |ctr: &Ctr| -> Option<File> {
        for _ in 0..600 {
            if let Some(f) = ctr.live().stdin.as_ref().and_then(|f| f.try_clone().ok()) {
                return Some(f);
            }
            let c = ctr.lock();
            if ctr.removed.load(Ordering::SeqCst) || (c.state.started_at > 0 && !c.running()) {
                return None;
            }
            drop(c);
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    };
    let mut out: Option<File> = None;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
        if n == 0 {
            if stdin_once {
                drop(out.take());
                ctr.live().stdin = None;
            }
            return;
        }
        if out.is_none() {
            out = handle(ctr);
        }
        match &mut out {
            Some(f) => {
                if f.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
            None => return,
        }
    }
}

/// Move output from the process pipes (or pty) into `sink` until all of
/// them reach EOF. A pty master reports EIO once the last slave closes.
fn pump(mut fds: Vec<(u8, File)>, mut sink: impl FnMut(u8, &[u8])) {
    let mut buf = vec![0u8; 32 << 10];
    while !fds.is_empty() {
        let mut pfds: Vec<libc::pollfd> = fds.iter().map(|(_, f)| libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
        // SAFETY: valid pollfd array.
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if r < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return;
        }
        let mut closed = Vec::new();
        for (i, p) in pfds.iter().enumerate() {
            if p.revents == 0 {
                continue;
            }
            match fds[i].1.read(&mut buf) {
                Ok(0) | Err(_) => closed.push(i),
                Ok(n) => sink(fds[i].0, &buf[..n]),
            }
        }
        for i in closed.into_iter().rev() {
            fds.remove(i);
        }
    }
}

/// Plaintext <-> TLS relay between a TCP client and a local socket pair,
/// so request handling is identical for unix and TLS clients.
fn tls_pump(mut tcp: std::net::TcpStream, mut conn: rustls::ServerConnection, mut local: UnixStream) {
    let mut buf = vec![0u8; 32 << 10];
    let _ = tcp.set_nodelay(true);
    loop {
        while conn.wants_write() {
            if conn.write_tls(&mut tcp).is_err() {
                return;
            }
        }
        let mut pfds = [
            libc::pollfd { fd: tcp.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: local.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: valid pollfd array.
        if unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) } < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return;
        }
        if pfds[0].revents != 0 {
            match conn.read_tls(&mut tcp) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if conn.process_new_packets().is_err() {
                let _ = conn.write_tls(&mut tcp);
                return;
            }
            loop {
                match conn.reader().read(&mut buf) {
                    Ok(0) => {
                        let _ = local.shutdown(Shutdown::Write);
                        break;
                    }
                    Ok(n) => {
                        if local.write_all(&buf[..n]).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => return,
                }
            }
        }
        if pfds[1].revents != 0 {
            match local.read(&mut buf) {
                Ok(0) | Err(_) => {
                    conn.send_close_notify();
                    while conn.wants_write() {
                        if conn.write_tls(&mut tcp).is_err() {
                            break;
                        }
                    }
                    return;
                }
                Ok(n) => {
                    if conn.writer().write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

fn serve_tls(daemon: Arc<Daemon>, port: u16) {
    let dir = Path::new(paths::TLS_DIR);
    let config = match pios_ctl::server::tls_config(dir) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            warn!("engine", "TLS disabled: {e}");
            return;
        }
    };
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            error!("engine", "bind :{port}: {e}");
            return;
        }
    };
    info!("engine", "API on tcp://0.0.0.0:{port} (mTLS)");
    for tcp in listener.incoming().flatten() {
        let Ok(conn) = rustls::ServerConnection::new(config.clone()) else { continue };
        let Ok((a, b)) = UnixStream::pair() else { continue };
        std::thread::spawn(move || tls_pump(tcp, conn, a));
        let d = daemon.clone();
        std::thread::spawn(move || d.serve(b));
    }
}

/// Stop every running container (system shutdown).
fn stop_all(daemon: &Arc<Daemon>) {
    daemon.shutting_down.store(true, Ordering::SeqCst);
    let running: Vec<Arc<Ctr>> = daemon.all().into_iter().filter(|c| c.lock().running()).collect();
    info!("engine", "stopping {} container(s)", running.len());
    let handles: Vec<_> = running
        .into_iter()
        .map(|c| {
            let d = daemon.clone();
            std::thread::spawn(move || {
                // Not a user stop: restart policies apply again at the next boot.
                let was = c.lock().manually_stopped;
                let _ = d.stop(&c, None, None);
                let mut g = c.lock();
                g.manually_stopped = was;
                c.save(&g);
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
}

pub fn run(cfg: &PiosConfig) -> Result<()> {
    // Block termination signals in every thread; the main thread waits for them.
    // SAFETY: plain signal mask manipulation before any thread exists.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    let root = PathBuf::from(paths::DATA_DIR).join("native");
    fs::create_dir_all(root.join("containers"))?;
    fs::create_dir_all(root.join("volumes"))?;
    network::cgroup_init()?;
    network::cgroup_cleanup();
    if let Err(e) = network::setup() {
        warn!("engine", "bridge network unavailable: {e}");
    }
    let store = Store::open(&root)?;
    let client = Client::new().map_err(|e| warn!("engine", "image pulls disabled: {e}")).ok();
    let daemon = Arc::new(Daemon {
        root: root.clone(),
        store,
        client,
        cfg: cfg.engine.clone(),
        hostname: cfg.hostname.clone(),
        containers: Mutex::new(BTreeMap::new()),
        execs: Mutex::new(HashMap::new()),
        networks: Mutex::new(Vec::new()),
        alloc: Mutex::new(()),
        shutting_down: AtomicBool::new(false),
        log_max: logs::parse_size(&cfg.engine.log_max_size).unwrap_or(10 << 20),
        log_files: cfg.engine.log_max_file.max(1),
    });
    let nets: Vec<Network> = fs::read(daemon.networks_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    for n in &nets {
        if let Err(e) = daemon.activate_network(n) {
            warn!("engine", "network {}: {e}", n.name);
        }
    }
    *daemon.networks.lock().unwrap() = nets;
    daemon.load_containers();

    let _ = fs::remove_file(DOCKER_SOCKET);
    let listener = UnixListener::bind(DOCKER_SOCKET).map_err(|e| Error::msg(format!("bind {DOCKER_SOCKET}: {e}")))?;
    let _ = fs::set_permissions(DOCKER_SOCKET, fs::Permissions::from_mode(0o660));
    let sock = std::ffi::CString::new(DOCKER_SOCKET).unwrap();
    // SAFETY: valid C string; gid 999 is the docker group in /etc/group.
    unsafe { libc::chown(sock.as_ptr(), 0, 999) };
    let d = daemon.clone();
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let d = d.clone();
            std::thread::spawn(move || d.serve(s));
        }
    });
    let tls_ready = ["ca.pem", "server-cert.pem", "server-key.pem"].iter().all(|f| Path::new(paths::TLS_DIR).join(f).exists());
    if cfg.engine.tls_tcp && tls_ready {
        let d = daemon.clone();
        std::thread::spawn(move || serve_tls(d, 2376));
    }
    info!("engine", "native engine ready ({} containers, {} images)", daemon.all().len(), daemon.store.image_ids().len());

    // Restart policies after a reboot or daemon restart.
    let d = daemon.clone();
    std::thread::spawn(move || {
        for c in d.all() {
            let restart = {
                let g = c.lock();
                g.should_restart(g.state.exit_code, true)
            };
            if restart {
                if let Err(e) = d.start(&c) {
                    error!("engine", "start {}: {}", &c.lock().id[..12], e.1);
                }
            }
        }
    });

    let mut sig: libc::c_int = 0;
    // SAFETY: valid set and out pointer.
    unsafe { libc::sigwait(&set, &mut sig) };
    info!("engine", "signal {sig}: shutting down");
    stop_all(&daemon);
    let _ = fs::remove_file(DOCKER_SOCKET);
    Ok(())
}
