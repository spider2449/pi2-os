//! Declarative description of a supervised service. Engines and system
//! modules produce `ServiceSpec`s; the supervisor runs them.

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceSpec {
    pub name: String,
    /// argv; `exec[0]` must be an absolute path.
    pub exec: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Services that must be ready before this one starts.
    pub after: Vec<String>,
    pub restart: RestartPolicy,
    pub ready: ReadyCheck,
    pub limits: CgroupLimits,
    pub stop_signal: i32,
    pub stop_timeout: Duration,
    /// Command run (and awaited, up to `stop_timeout`) before the service is
    /// signalled during system shutdown, e.g. to stop containers gracefully.
    pub shutdown_hook: Option<Vec<String>>,
}

impl ServiceSpec {
    pub fn new(name: impl Into<String>, exec: &[&str]) -> Self {
        Self {
            name: name.into(),
            exec: exec.iter().map(|s| s.to_string()).collect(),
            env: vec![("PATH".into(), DEFAULT_PATH.into())],
            after: Vec::new(),
            restart: RestartPolicy::Always,
            ready: ReadyCheck::Started,
            limits: CgroupLimits::default(),
            stop_signal: SIGTERM,
            stop_timeout: Duration::from_secs(10),
            shutdown_hook: None,
        }
    }

    pub fn after(mut self, deps: &[&str]) -> Self {
        self.after.extend(deps.iter().map(|s| s.to_string()));
        self
    }

    pub fn ready(mut self, ready: ReadyCheck) -> Self {
        self.ready = ready;
        self
    }

    pub fn restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }

    pub fn limits(mut self, limits: CgroupLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn shutdown_hook(mut self, argv: &[&str]) -> Self {
        self.shutdown_hook = Some(argv.iter().map(|s| s.to_string()).collect());
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn stop_timeout(mut self, t: Duration) -> Self {
        self.stop_timeout = t;
        self
    }
}

pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
pub const SIGTERM: i32 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestartPolicy {
    /// Restart whenever the process exits.
    Always,
    /// Restart only on non-zero exit; exit 0 marks the service done (and ready).
    OnFailure,
    /// Never restart.
    Never,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReadyCheck {
    /// Ready as soon as the process has been spawned.
    Started,
    /// Ready once the given path exists (socket, marker file).
    PathExists(String),
    /// Ready when `GET <path>` over the unix socket answers HTTP 200.
    HttpUnix { socket: String, path: String },
    /// Ready only after the process exits with status 0 (oneshot).
    Exited,
}

/// cgroup v2 resource controls applied to the service's cgroup.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CgroupLimits {
    /// memory.max, e.g. "256M".
    pub memory_max: Option<String>,
    /// cpu.weight (1..=10000, default 100).
    pub cpu_weight: Option<u32>,
    /// pids.max.
    pub pids_max: Option<u32>,
}
