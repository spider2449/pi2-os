//! Container model: the Docker create request, how it combines with the
//! image configuration, the persisted container record and the JSON views
//! (`/containers/json`, `/containers/{id}/json`) the docker CLI expects.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

use crate::distribution::ImageRunConfig;
use crate::util::{human_duration, now_ns, rfc3339};

/// `Cmd`/`Entrypoint` may be a JSON string or an array.
fn str_or_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        S(String),
        A(Vec<String>),
    }
    Ok(match Option::<V>::deserialize(d)? {
        None => None,
        Some(V::S(s)) => Some(vec![s]),
        Some(V::A(a)) => Some(a),
    })
}

/// Body of `POST /containers/create` (fields PiOS acts on; others ignored).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct CreateRequest {
    pub hostname: String,
    pub domainname: String,
    pub user: String,
    pub attach_stdin: bool,
    pub attach_stdout: bool,
    pub attach_stderr: bool,
    pub tty: bool,
    pub open_stdin: bool,
    pub stdin_once: bool,
    pub env: Option<Vec<String>>,
    #[serde(deserialize_with = "str_or_vec")]
    pub cmd: Option<Vec<String>>,
    #[serde(deserialize_with = "str_or_vec")]
    pub entrypoint: Option<Vec<String>>,
    pub image: String,
    pub working_dir: String,
    pub labels: Option<BTreeMap<String, String>>,
    pub exposed_ports: Option<serde_json::Map<String, Value>>,
    pub stop_signal: Option<String>,
    pub stop_timeout: Option<i64>,
    pub host_config: HostConfig,
    pub networking_config: Option<NetworkingConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct NetworkingConfig {
    pub endpoints_config: Option<BTreeMap<String, EndpointSettings>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct EndpointSettings {
    pub aliases: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HostConfig {
    pub binds: Option<Vec<String>>,
    pub mounts: Option<Vec<MountRequest>>,
    pub tmpfs: Option<BTreeMap<String, String>>,
    pub network_mode: String,
    pub port_bindings: Option<BTreeMap<String, Option<Vec<PortBinding>>>>,
    pub publish_all_ports: bool,
    pub restart_policy: RestartPolicy,
    pub auto_remove: bool,
    pub privileged: bool,
    pub cap_add: Option<Vec<String>>,
    pub cap_drop: Option<Vec<String>>,
    pub memory: i64,
    pub memory_swap: i64,
    pub nano_cpus: i64,
    pub cpu_shares: i64,
    pub cpu_quota: i64,
    pub cpu_period: i64,
    pub cpuset_cpus: String,
    pub pids_limit: Option<i64>,
    pub readonly_rootfs: bool,
    pub dns: Option<Vec<String>>,
    pub extra_hosts: Option<Vec<String>>,
    pub devices: Option<Vec<DeviceMapping>>,
    pub init: Option<bool>,
    pub shm_size: i64,
    pub sysctls: Option<BTreeMap<String, String>>,
    pub ulimits: Option<Vec<Ulimit>>,
    pub security_opt: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct PortBinding {
    pub host_ip: String,
    pub host_port: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct RestartPolicy {
    pub name: String,
    pub maximum_retry_count: i64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct MountRequest {
    #[serde(rename = "Type")]
    pub kind: String,
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct DeviceMapping {
    pub path_on_host: String,
    pub path_in_container: String,
    pub cgroup_permissions: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct Ulimit {
    pub name: String,
    pub soft: i64,
    pub hard: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetMode {
    Bridge,
    Host,
    None,
}

impl NetMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "" | "default" | "bridge" => Ok(Self::Bridge),
            "host" => Ok(Self::Host),
            "none" => Ok(Self::None),
            other => Err(format!("network mode {other:?} is not supported (bridge, host or none)")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Bridge => "bridge",
            Self::Host => "host",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    pub fn name(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// A published port. `host_port` 0 until allocated at start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMap {
    pub proto: Proto,
    pub container_port: u16,
    pub host_ip: Option<Ipv4Addr>,
    pub host_port: u16,
}

pub fn parse_port_bindings(b: &BTreeMap<String, Option<Vec<PortBinding>>>) -> Result<Vec<PortMap>, String> {
    let mut out = Vec::new();
    for (key, binds) in b {
        let (port, proto) = key.split_once('/').unwrap_or((key, "tcp"));
        let proto = match proto {
            "tcp" => Proto::Tcp,
            "udp" => Proto::Udp,
            p => return Err(format!("unsupported protocol {p}")),
        };
        let container_port: u16 = port.parse().map_err(|_| format!("bad container port {key:?} (ranges are not supported)"))?;
        for pb in binds.iter().flatten() {
            let host_ip = match pb.host_ip.as_str() {
                "" | "0.0.0.0" => None,
                "::" => None,
                ip => Some(ip.parse().map_err(|_| format!("bad host IP {ip:?} (IPv4 only)"))?),
            };
            let host_port = if pb.host_port.is_empty() {
                0
            } else {
                pb.host_port.parse().map_err(|_| format!("bad host port {:?} (ranges are not supported)", pb.host_port))?
            };
            let m = PortMap { proto, container_port, host_ip, host_port };
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountKind {
    Bind,
    Volume,
    Tmpfs,
}

/// A mount into the container, resolved from Binds/Mounts/Tmpfs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountPoint {
    pub kind: MountKind,
    /// Host path (bind), volume name (volume), options (tmpfs).
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

/// `-v` syntax: `/host:/ctr[:ro]`, `name:/ctr[:ro]`, or `/ctr` (anonymous volume).
pub fn parse_bind(spec: &str) -> Result<MountPoint, String> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (source, target, opts) = match parts.as_slice() {
        [t] => ("", *t, ""),
        [s, t] => (*s, *t, ""),
        [s, t, o] => (*s, *t, *o),
        _ => return Err(format!("invalid volume specification: {spec:?}")),
    };
    if !target.starts_with('/') {
        return Err(format!("invalid volume specification: {spec:?}: destination must be absolute"));
    }
    let mut read_only = false;
    for o in opts.split(',').filter(|o| !o.is_empty()) {
        match o {
            "ro" => read_only = true,
            "rw" | "z" | "Z" | "rprivate" | "private" | "rslave" | "slave" | "nocopy" => {}
            other => return Err(format!("unsupported volume option {other:?}")),
        }
    }
    let kind = if source.starts_with('/') { MountKind::Bind } else { MountKind::Volume };
    if kind == MountKind::Volume && !source.is_empty() && !valid_volume_name(source) {
        return Err(format!("invalid volume name {source:?}"));
    }
    Ok(MountPoint { kind, source: source.to_string(), target: clean_path(target), read_only })
}

pub fn valid_volume_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// Container names follow the same rule as Docker: [a-zA-Z0-9][a-zA-Z0-9_.-]+.
pub fn valid_container_name(name: &str) -> bool {
    name.len() >= 2 && valid_volume_name(name)
}

/// Normalise an absolute container path ("/a//b/" -> "/a/b"); rejects "..".
pub fn clean_path(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty() && *s != "." && *s != "..").collect();
    format!("/{}", parts.join("/"))
}

pub fn resolve_mounts(h: &HostConfig) -> Result<Vec<MountPoint>, String> {
    let mut out: Vec<MountPoint> = Vec::new();
    for b in h.binds.iter().flatten() {
        out.push(parse_bind(b)?);
    }
    for m in h.mounts.iter().flatten() {
        let target = clean_path(&m.target);
        let kind = match m.kind.as_str() {
            "bind" => MountKind::Bind,
            "volume" => MountKind::Volume,
            "tmpfs" => MountKind::Tmpfs,
            k => return Err(format!("mount type {k:?} is not supported")),
        };
        if kind == MountKind::Bind && !m.source.starts_with('/') {
            return Err(format!("bind source must be absolute: {:?}", m.source));
        }
        out.push(MountPoint { kind, source: m.source.clone(), target, read_only: m.read_only });
    }
    for (target, opts) in h.tmpfs.iter().flatten() {
        out.push(MountPoint { kind: MountKind::Tmpfs, source: opts.clone(), target: clean_path(target), read_only: false });
    }
    let mut seen = std::collections::BTreeSet::new();
    for m in &out {
        if !seen.insert(m.target.clone()) {
            return Err(format!("duplicate mount point: {}", m.target));
        }
    }
    Ok(out)
}

/// What to execute, after combining the request with the image defaults.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessConfig {
    pub path: String,
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub user: String,
}

/// Docker's rules: a request Entrypoint replaces the image's and discards the
/// image Cmd; a request Cmd replaces the image Cmd; env merges by key.
pub fn process_config(req: &CreateRequest, image: &ImageRunConfig) -> Result<ProcessConfig, String> {
    let entrypoint = match &req.entrypoint {
        // `--entrypoint ""` resets it.
        Some(e) if e.len() == 1 && e[0].is_empty() => Vec::new(),
        Some(e) => e.clone(),
        None => image.entrypoint.clone().unwrap_or_default(),
    };
    let cmd = match (&req.cmd, &req.entrypoint) {
        (Some(c), _) => c.clone(),
        (None, Some(_)) => Vec::new(),
        (None, None) => image.cmd.clone().unwrap_or_default(),
    };
    let argv: Vec<String> = entrypoint.into_iter().chain(cmd).collect();
    let Some((path, args)) = argv.split_first() else {
        return Err("no command specified".into());
    };
    let mut env: Vec<String> = Vec::new();
    let mut put = |kv: &str| {
        let key = kv.split('=').next().unwrap_or(kv);
        env.retain(|e| e.split('=').next() != Some(key));
        env.push(kv.to_string());
    };
    for e in image.env.iter().flatten() {
        put(e);
    }
    for e in req.env.iter().flatten() {
        // `-e NAME` without a value is resolved by the CLI; drop leftovers.
        if e.contains('=') {
            put(e);
        }
    }
    if !env.iter().any(|e| e.starts_with("PATH=")) {
        env.insert(0, "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
    }
    let cwd = if !req.working_dir.is_empty() { req.working_dir.clone() } else { image.working_dir.clone().unwrap_or_default() };
    let cwd = if cwd.is_empty() { "/".into() } else { clean_path(&cwd) };
    let user = if !req.user.is_empty() { req.user.clone() } else { image.user.clone().unwrap_or_default() };
    Ok(ProcessConfig { path: path.clone(), args: args.to_vec(), env, cwd, user })
}

/// The capability set of a container. Docker's default set unless privileged,
/// adjusted by CapAdd/CapDrop ("ALL" supported).
pub fn capabilities(h: &HostConfig) -> Result<Vec<u32>, String> {
    const DEFAULT: [&str; 14] = [
        "CHOWN", "DAC_OVERRIDE", "FSETID", "FOWNER", "MKNOD", "NET_RAW", "SETGID", "SETUID", "SETFCAP", "SETPCAP",
        "NET_BIND_SERVICE", "SYS_CHROOT", "KILL", "AUDIT_WRITE",
    ];
    let all: Vec<u32> = (0..=40).collect();
    let mut caps: Vec<u32> = if h.privileged { all.clone() } else { DEFAULT.iter().filter_map(|c| cap_number(c)).collect() };
    for c in h.cap_add.iter().flatten() {
        if c.eq_ignore_ascii_case("ALL") {
            caps = all.clone();
        } else {
            let n = cap_number(c).ok_or_else(|| format!("unknown capability {c:?}"))?;
            if !caps.contains(&n) {
                caps.push(n);
            }
        }
    }
    for c in h.cap_drop.iter().flatten() {
        if c.eq_ignore_ascii_case("ALL") {
            caps.clear();
        } else {
            let n = cap_number(c).ok_or_else(|| format!("unknown capability {c:?}"))?;
            caps.retain(|x| *x != n);
        }
    }
    caps.sort_unstable();
    Ok(caps)
}

/// The default seccomp filter applies unless privileged or
/// `--security-opt seccomp=unconfined`.
pub fn seccomp_enabled(h: &HostConfig) -> bool {
    !h.privileged && !h.security_opt.iter().flatten().any(|o| matches!(o.as_str(), "seccomp=unconfined" | "seccomp:unconfined"))
}

pub fn cap_number(name: &str) -> Option<u32> {
    const NAMES: [&str; 41] = [
        "CHOWN", "DAC_OVERRIDE", "DAC_READ_SEARCH", "FOWNER", "FSETID", "KILL", "SETGID", "SETUID", "SETPCAP",
        "LINUX_IMMUTABLE", "NET_BIND_SERVICE", "NET_BROADCAST", "NET_ADMIN", "NET_RAW", "IPC_LOCK", "IPC_OWNER",
        "SYS_MODULE", "SYS_RAWIO", "SYS_CHROOT", "SYS_PTRACE", "SYS_PACCT", "SYS_ADMIN", "SYS_BOOT", "SYS_NICE",
        "SYS_RESOURCE", "SYS_TIME", "SYS_TTY_CONFIG", "MKNOD", "LEASE", "AUDIT_WRITE", "AUDIT_CONTROL", "SETFCAP",
        "MAC_OVERRIDE", "MAC_ADMIN", "SYSLOG", "WAKE_ALARM", "BLOCK_SUSPEND", "AUDIT_READ", "PERFMON", "BPF",
        "CHECKPOINT_RESTORE",
    ];
    let n = name.to_ascii_uppercase();
    let n = n.strip_prefix("CAP_").unwrap_or(&n);
    NAMES.iter().position(|c| *c == n).map(|i| i as u32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Created,
    Running,
    Restarting,
    Exited,
    Removing,
    Dead,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Exited => "exited",
            Self::Removing => "removing",
            Self::Dead => "dead",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub status: Status,
    pub pid: i32,
    pub exit_code: i32,
    pub error: String,
    pub oom_killed: bool,
    pub started_at: i64,
    pub finished_at: i64,
}

impl Default for State {
    fn default() -> Self {
        Self { status: Status::Created, pid: 0, exit_code: 0, error: String::new(), oom_killed: false, started_at: 0, finished_at: 0 }
    }
}

/// Persisted container record (`containers/<id>/container.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub created: i64,
    /// Image as the user named it, e.g. "alpine".
    pub image: String,
    /// "sha256:<config digest>".
    pub image_id: String,
    /// Layer directory names (diff ids, hex), lowest first.
    pub layers: Vec<String>,
    pub request: CreateRequest,
    pub process: ProcessConfig,
    pub hostname: String,
    pub net: NetMode,
    pub ports: Vec<PortMap>,
    pub mounts: Vec<MountPoint>,
    pub state: State,
    pub restart_count: u32,
    /// Stopped by the user: `unless-stopped` and `always` do not restart it
    /// (`always` does again after a daemon restart, as in Docker).
    pub manually_stopped: bool,
    pub ip: Option<Ipv4Addr>,
    /// User-defined network id (bridge networking); None is the default bridge.
    #[serde(default)]
    pub network: Option<String>,
    /// Extra DNS names on that network (compose service names).
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// The network a container is attached to, for the JSON views.
pub struct NetInfo<'a> {
    pub name: &'a str,
    pub id: &'a str,
    pub gateway: Ipv4Addr,
    pub prefix: u8,
}

impl Container {
    pub fn short_id(&self) -> &str {
        &self.id[..12]
    }

    pub fn running(&self) -> bool {
        matches!(self.state.status, Status::Running | Status::Restarting)
    }

    pub fn tty(&self) -> bool {
        self.request.tty
    }

    /// Restart after the process exited with `code`?
    pub fn should_restart(&self, code: i32, daemon_start: bool) -> bool {
        let p = &self.request.host_config.restart_policy;
        match p.name.as_str() {
            "always" => daemon_start || !self.manually_stopped,
            "unless-stopped" => !self.manually_stopped,
            "on-failure" => {
                !daemon_start
                    && !self.manually_stopped
                    && code != 0
                    && (p.maximum_retry_count <= 0 || (self.restart_count as i64) < p.maximum_retry_count)
            }
            _ => false,
        }
    }

    fn command_line(&self) -> String {
        std::iter::once(self.process.path.as_str())
            .chain(self.process.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn status_text(&self, now: i64) -> String {
        let ago = |t: i64| human_duration((now - t) / 1_000_000_000);
        match self.state.status {
            Status::Running => format!("Up {}", ago(self.state.started_at)),
            Status::Restarting => format!("Restarting ({}) {} ago", self.state.exit_code, ago(self.state.finished_at)),
            Status::Exited if self.state.finished_at > 0 => {
                format!("Exited ({}) {} ago", self.state.exit_code, ago(self.state.finished_at))
            }
            Status::Exited => format!("Exited ({})", self.state.exit_code),
            Status::Created => "Created".into(),
            Status::Removing => "Removal In Progress".into(),
            Status::Dead => "Dead".into(),
        }
    }

    fn ports_inspect(&self) -> Value {
        let mut m = serde_json::Map::new();
        for p in self.exposed_ports() {
            m.entry(p).or_insert(Value::Null);
        }
        for p in &self.ports {
            let key = format!("{}/{}", p.container_port, p.proto.name());
            let ip = p.host_ip.map_or("0.0.0.0".to_string(), |i| i.to_string());
            let entry = json!({ "HostIp": ip, "HostPort": p.host_port.to_string() });
            match m.get_mut(&key) {
                Some(Value::Array(a)) => a.push(entry),
                _ => {
                    m.insert(key, json!([entry]));
                }
            }
        }
        Value::Object(m)
    }

    fn exposed_ports(&self) -> Vec<String> {
        self.request.exposed_ports.iter().flat_map(|m| m.keys().cloned()).collect()
    }

    fn networks(&self, n: &NetInfo) -> Value {
        let ip = self.ip.map(|i| i.to_string()).unwrap_or_default();
        let (gw, len) = if self.ip.is_some() { (n.gateway.to_string(), n.prefix) } else { (String::new(), 0) };
        let mut aliases = self.aliases.clone();
        if self.network.is_some() {
            aliases.push(self.short_id().to_string());
        }
        json!({ n.name: {
            "Aliases": aliases, "DNSNames": aliases,
            "IPAddress": ip, "IPPrefixLen": len, "Gateway": gw,
            "MacAddress": self.ip.map(mac_for).unwrap_or_default(),
            "NetworkID": n.id, "EndpointID": if self.running() { self.id.as_str() } else { "" },
        }})
    }

    fn mounts_json(&self, volumes_dir: &str) -> Value {
        Value::Array(
            self.mounts
                .iter()
                .map(|m| match m.kind {
                    MountKind::Bind => json!({"Type": "bind", "Source": m.source, "Destination": m.target, "Mode": if m.read_only { "ro" } else { "" }, "RW": !m.read_only, "Propagation": "rprivate"}),
                    MountKind::Volume => json!({"Type": "volume", "Name": m.source, "Source": format!("{volumes_dir}/{}/_data", m.source), "Destination": m.target, "Driver": "local", "Mode": if m.read_only { "ro" } else { "z" }, "RW": !m.read_only, "Propagation": ""}),
                    MountKind::Tmpfs => json!({"Type": "tmpfs", "Source": "", "Destination": m.target, "Mode": m.source, "RW": true, "Propagation": ""}),
                })
                .collect(),
        )
    }

    /// `GET /containers/json` entry.
    pub fn summary(&self, n: &NetInfo, volumes_dir: &str) -> Value {
        let ports: Vec<Value> = self
            .ports
            .iter()
            .map(|p| json!({"IP": p.host_ip.map_or("0.0.0.0".to_string(), |i| i.to_string()), "PrivatePort": p.container_port, "PublicPort": p.host_port, "Type": p.proto.name()}))
            .collect();
        json!({
            "Id": self.id, "Names": [format!("/{}", self.name)], "Image": self.image, "ImageID": self.image_id,
            "Command": self.command_line(), "Created": self.created / 1_000_000_000, "Ports": ports,
            "Labels": self.request.labels.clone().unwrap_or_default(), "State": self.state.status.name(),
            "Status": self.status_text(now_ns()), "HostConfig": {"NetworkMode": n.name},
            "NetworkSettings": {"Networks": self.networks(n)}, "Mounts": self.mounts_json(volumes_dir),
        })
    }

    /// `GET /containers/{id}/json`.
    pub fn inspect(&self, n: &NetInfo, volumes_dir: &str, root: &str) -> Value {
        let r = &self.request;
        let s = &self.state;
        let mut host = serde_json::to_value(&r.host_config).unwrap_or(Value::Null);
        if let Some(h) = host.as_object_mut() {
            h.insert("NetworkMode".into(), json!(n.name));
            h.insert("LogConfig".into(), json!({"Type": "local", "Config": {}}));
        }
        json!({
            "Id": self.id, "Created": rfc3339(self.created), "Path": self.process.path, "Args": self.process.args,
            "State": {
                "Status": s.status.name(), "Running": self.running(), "Paused": false,
                "Restarting": s.status == Status::Restarting, "OOMKilled": s.oom_killed, "Dead": s.status == Status::Dead,
                "Pid": s.pid, "ExitCode": s.exit_code, "Error": s.error,
                "StartedAt": rfc3339(s.started_at), "FinishedAt": rfc3339(s.finished_at),
            },
            "Image": self.image_id, "Name": format!("/{}", self.name), "RestartCount": self.restart_count,
            "Driver": "overlay2", "Platform": "linux", "MountLabel": "", "ProcessLabel": "", "AppArmorProfile": "",
            "HostnamePath": format!("{root}/hostname"), "HostsPath": format!("{root}/hosts"),
            "ResolvConfPath": format!("{root}/resolv.conf"), "LogPath": format!("{root}/container.log"),
            "HostConfig": host, "Mounts": self.mounts_json(volumes_dir),
            "Config": {
                "Hostname": self.hostname, "Domainname": r.domainname, "User": self.process.user,
                "AttachStdin": r.attach_stdin, "AttachStdout": r.attach_stdout, "AttachStderr": r.attach_stderr,
                "Tty": r.tty, "OpenStdin": r.open_stdin, "StdinOnce": r.stdin_once,
                "Env": self.process.env, "Cmd": r.cmd, "Entrypoint": r.entrypoint, "Image": self.image,
                "WorkingDir": self.process.cwd, "Labels": r.labels.clone().unwrap_or_default(),
                "ExposedPorts": r.exposed_ports, "StopSignal": r.stop_signal, "StopTimeout": r.stop_timeout,
            },
            "NetworkSettings": {
                "Ports": self.ports_inspect(), "IPAddress": self.ip.map(|i| i.to_string()).unwrap_or_default(),
                "IPPrefixLen": if self.ip.is_some() { n.prefix } else { 0 },
                "Gateway": if self.ip.is_some() { n.gateway.to_string() } else { String::new() },
                "MacAddress": self.ip.map(mac_for).unwrap_or_default(),
                "Networks": self.networks(n), "SandboxKey": "",
            },
        })
    }
}

/// Deterministic MAC from the IP, like Docker (02:42 + IPv4).
pub fn mac_for(ip: Ipv4Addr) -> String {
    let o = ip.octets();
    format!("02:42:{:02x}:{:02x}:{:02x}:{:02x}", o[0], o[1], o[2], o[3])
}

/// Lowest free host address in `net/prefix`, skipping network, gateway
/// (.1) and broadcast.
pub fn allocate_ip(net: Ipv4Addr, prefix: u8, used: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    let base = u32::from(net) & (u32::MAX << (32 - prefix as u32));
    let size = 1u32 << (32 - prefix as u32);
    (2..size - 1).map(|i| Ipv4Addr::from(base + i)).find(|ip| !used.contains(ip))
}

/// /etc/hosts for a container.
pub fn hosts_file(hostname: &str, ip: Option<Ipv4Addr>, extra: &[String]) -> String {
    let mut s = String::from(
        "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\nfe00::0\tip6-localnet\nff00::0\tip6-mcastprefix\nff02::1\tip6-allnodes\nff02::2\tip6-allrouters\n",
    );
    for e in extra {
        // --add-host name:ip
        if let Some((name, addr)) = e.split_once(':') {
            s.push_str(&format!("{addr}\t{name}\n"));
        }
    }
    if let Some(ip) = ip {
        s.push_str(&format!("{ip}\t{hostname}\n"));
    }
    s
}

/// resolv.conf for a bridged container: the host's, minus loopback servers
/// (unreachable from another network namespace), or Docker's fallback.
pub fn resolv_conf(host: &str, dns: &[String]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut servers = 0;
    if dns.is_empty() {
        for l in host.lines() {
            let t = l.trim();
            if let Some(ns) = t.strip_prefix("nameserver") {
                let ns = ns.trim();
                if ns.starts_with("127.") || ns == "::1" {
                    continue;
                }
                servers += 1;
            }
            lines.push(t.to_string());
        }
    } else {
        for d in dns {
            lines.push(format!("nameserver {d}"));
            servers += 1;
        }
        lines.extend(host.lines().filter(|l| l.trim().starts_with("search") || l.trim().starts_with("options")).map(str::to_string));
    }
    if servers == 0 {
        lines.push("nameserver 8.8.8.8".into());
        lines.push("nameserver 8.8.4.4".into());
    }
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> ImageRunConfig {
        ImageRunConfig {
            env: Some(vec!["PATH=/usr/bin:/bin".into(), "LANG=C".into()]),
            cmd: Some(vec!["/bin/sh".into()]),
            working_dir: Some("/app".into()),
            ..Default::default()
        }
    }

    #[test]
    fn combines_request_with_image_defaults() {
        let req: CreateRequest =
            serde_json::from_str(r#"{"Image":"alpine","Cmd":["echo","hi"],"Env":["LANG=en","X=1","NOVALUE"]}"#).unwrap();
        let p = process_config(&req, &image()).unwrap();
        assert_eq!((p.path.as_str(), p.args.as_slice()), ("echo", &["hi".to_string()][..]));
        assert_eq!(p.env, vec!["PATH=/usr/bin:/bin", "LANG=en", "X=1"]);
        assert_eq!(p.cwd, "/app");
        // An Entrypoint from the request drops the image Cmd.
        let req: CreateRequest = serde_json::from_str(r#"{"Image":"x","Entrypoint":"/bin/date"}"#).unwrap();
        let p = process_config(&req, &image()).unwrap();
        assert_eq!((p.path.as_str(), p.args.len()), ("/bin/date", 0));
        // No command at all is an error, as in Docker.
        let empty = ImageRunConfig::default();
        assert!(process_config(&CreateRequest::default(), &empty).is_err());
        // PATH is supplied when the image has none.
        let p = process_config(&serde_json::from_str(r#"{"Cmd":["true"]}"#).unwrap(), &empty).unwrap();
        assert!(p.env[0].starts_with("PATH="));
    }

    #[test]
    fn parses_binds_and_ports() {
        assert_eq!(
            parse_bind("/srv/data:/data:ro").unwrap(),
            MountPoint { kind: MountKind::Bind, source: "/srv/data".into(), target: "/data".into(), read_only: true }
        );
        assert_eq!(parse_bind("cache:/var/cache/").unwrap().kind, MountKind::Volume);
        assert_eq!(parse_bind("/anon").unwrap().source, "");
        assert!(parse_bind("rel:dir").is_err());
        assert!(parse_bind("/a:/b:bogus").is_err());
        let mut pb = BTreeMap::new();
        pb.insert("80/tcp".to_string(), Some(vec![PortBinding { host_ip: "".into(), host_port: "8080".into() }]));
        pb.insert("53/udp".to_string(), Some(vec![PortBinding { host_ip: "127.0.0.1".into(), host_port: "".into() }]));
        let ports = parse_port_bindings(&pb).unwrap();
        assert_eq!(ports.len(), 2);
        assert!(ports.contains(&PortMap { proto: Proto::Tcp, container_port: 80, host_ip: None, host_port: 8080 }));
        assert!(ports.contains(&PortMap { proto: Proto::Udp, container_port: 53, host_ip: Some(Ipv4Addr::LOCALHOST), host_port: 0 }));
    }

    #[test]
    fn capability_sets() {
        let mut h = HostConfig::default();
        let caps = capabilities(&h).unwrap();
        assert_eq!(caps.len(), 14);
        assert!(!caps.contains(&cap_number("SYS_ADMIN").unwrap()));
        h.cap_add = Some(vec!["NET_ADMIN".into()]);
        h.cap_drop = Some(vec!["CAP_MKNOD".into()]);
        let caps = capabilities(&h).unwrap();
        assert!(caps.contains(&12) && !caps.contains(&27));
        h.privileged = true;
        h.cap_add = None;
        h.cap_drop = None;
        assert_eq!(capabilities(&h).unwrap().len(), 41);
        h.cap_add = Some(vec!["BOGUS".into()]);
        assert!(capabilities(&h).is_err());
    }

    #[test]
    fn restart_policies() {
        let req: CreateRequest = serde_json::from_str(r#"{"HostConfig":{"RestartPolicy":{"Name":"on-failure","MaximumRetryCount":2}}}"#).unwrap();
        let mut c = Container {
            id: "a".repeat(64), name: "x".into(), created: 0, image: "alpine".into(), image_id: String::new(),
            layers: vec![], request: req, process: ProcessConfig::default(), hostname: "h".into(), net: NetMode::Bridge,
            ports: vec![], mounts: vec![], state: State::default(), restart_count: 0, manually_stopped: false, ip: None,
            network: None, aliases: vec![],
        };
        assert!(c.should_restart(1, false) && !c.should_restart(0, false));
        c.restart_count = 2;
        assert!(!c.should_restart(1, false));
        c.request.host_config.restart_policy.name = "always".into();
        c.manually_stopped = true;
        assert!(!c.should_restart(0, false) && c.should_restart(0, true));
        c.request.host_config.restart_policy.name = "unless-stopped".into();
        assert!(!c.should_restart(0, true));
        c.request.host_config.restart_policy.name = "".into();
        c.manually_stopped = false;
        assert!(!c.should_restart(1, false));
    }

    #[test]
    fn ip_allocation_and_files() {
        let net: Ipv4Addr = "172.17.0.0".parse().unwrap();
        assert_eq!(allocate_ip(net, 16, &[]), Some("172.17.0.2".parse().unwrap()));
        assert_eq!(allocate_ip(net, 30, &["172.17.0.2".parse().unwrap()]), None);
        assert_eq!(mac_for("172.17.0.2".parse().unwrap()), "02:42:ac:11:00:02");
        let r = resolv_conf("nameserver 127.0.0.53\nsearch lan\n", &[]);
        assert!(r.contains("search lan") && r.contains("8.8.8.8") && !r.contains("127.0.0.53"));
        let r = resolv_conf("nameserver 192.168.1.1\n", &["1.1.1.1".into()]);
        assert_eq!(r, "nameserver 1.1.1.1\n");
        let h = hosts_file("abc", Some("172.17.0.2".parse().unwrap()), &["db:10.0.0.5".into()]);
        assert!(h.contains("10.0.0.5\tdb\n") && h.ends_with("172.17.0.2\tabc\n"));
    }

    #[test]
    fn names_and_paths() {
        assert!(valid_container_name("web-1") && !valid_container_name("-x") && !valid_container_name("a"));
        assert_eq!(clean_path("/a//b/../c/"), "/a/b/c");
    }
}
