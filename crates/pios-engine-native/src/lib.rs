//! Rust-native container engine: one daemon (`pios engined`) that speaks the
//! Docker Engine API subset the docker CLI uses, pulls images itself and
//! starts containers through a tiny helper (`pios container-init`) instead
//! of dockerd + containerd + shim + runc.
//!
//! Pure parts (references, HTTP framing, registry formats, the container
//! model, logs) are host testable; the daemon and runtime need Linux.

use std::time::Duration;

use pios_core::error::IoContext;
use pios_core::service::{CgroupLimits, ReadyCheck, RestartPolicy, ServiceSpec};
use pios_core::{paths, Result};
use pios_engine_api::{ContainerEngine, EngineContext};

pub mod distribution;
pub mod dns;
pub mod http;
pub mod logs;
pub mod netmodel;
pub mod reference;
pub mod state;
pub mod util;

#[cfg(target_os = "linux")]
mod client;
#[cfg(target_os = "linux")]
pub mod daemon;
#[cfg(target_os = "linux")]
mod netlink;
#[cfg(target_os = "linux")]
mod network;
#[cfg(target_os = "linux")]
mod registry;
#[cfg(target_os = "linux")]
pub mod runtime;
#[cfg(target_os = "linux")]
mod seccomp;
#[cfg(target_os = "linux")]
mod store;

/// Same socket path as dockerd, so every Docker client works unchanged.
pub const DOCKER_SOCKET: &str = "/var/run/docker.sock";

pub struct NativeEngine;

impl ContainerEngine for NativeEngine {
    fn name(&self) -> &'static str {
        "native"
    }

    fn prepare(&self, ctx: &EngineContext) -> Result<()> {
        for d in ["containers", "volumes", "images", "layers"] {
            let p = ctx.data_dir.join("native").join(d);
            std::fs::create_dir_all(&p).ctx(|| format!("mkdir {}", p.display()))?;
        }
        Ok(())
    }

    fn services(&self, ctx: &EngineContext) -> Vec<ServiceSpec> {
        let limits = CgroupLimits { memory_max: ctx.config.daemon_memory_max.clone(), cpu_weight: Some(200), pids_max: None };
        let deps: Vec<&str> = ctx.depends_on.iter().map(String::as_str).collect();
        // engined stops its containers itself on SIGTERM; give it the time.
        vec![ServiceSpec::new("engined", &[paths::PIOS_BIN, "engined"])
            .after(&deps)
            .ready(ReadyCheck::HttpUnix { socket: DOCKER_SOCKET.into(), path: "/_ping".into() })
            .restart(RestartPolicy::Always)
            .limits(limits)
            .stop_timeout(Duration::from_secs(30))]
    }

    fn ready_service(&self) -> &'static str {
        "engined"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pios_core::config::EngineConfig;

    #[test]
    fn one_daemon_with_api_readiness() {
        let ctx = EngineContext {
            config: EngineConfig::default(),
            data_dir: "/data".into(),
            run_dir: "/run/pios".into(),
            boot_dir: "/boot".into(),
            depends_on: vec!["netd".into(), "ntpd".into()],
        };
        let s = NativeEngine.services(&ctx);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, NativeEngine.ready_service());
        assert_eq!(s[0].after, vec!["netd", "ntpd"]);
        assert!(matches!(&s[0].ready, ReadyCheck::HttpUnix { path, .. } if path == "/_ping"));
    }
}
