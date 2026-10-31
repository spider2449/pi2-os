//! Docker engine module: supervises containerd and dockerd (static armhf
//! binaries in /usr/bin) with a configuration tuned for the Pi 2.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Map, Value};

use pios_core::config::EngineConfig;
use pios_core::error::IoContext;
use pios_core::service::{CgroupLimits, ReadyCheck, RestartPolicy, ServiceSpec};
use pios_core::{paths, Result};
use pios_engine_api::{ContainerEngine, EngineContext};

#[cfg(target_os = "linux")]
pub mod api;

pub const DOCKER_SOCKET: &str = "/var/run/docker.sock";
pub const CONTAINERD_SOCKET: &str = "/run/containerd/containerd.sock";
/// Mutual TLS material expected on the boot partition.
pub const TLS_DIR: &str = "docker-tls";
const TLS_FILES: [&str; 3] = ["ca.pem", "server-cert.pem", "server-key.pem"];

pub struct DockerEngine;

impl DockerEngine {
    fn daemon_json_path(ctx: &EngineContext) -> std::path::PathBuf {
        ctx.run_dir.join("docker/daemon.json")
    }
}

impl ContainerEngine for DockerEngine {
    fn name(&self) -> &'static str {
        "docker"
    }

    fn prepare(&self, ctx: &EngineContext) -> Result<()> {
        let tls_dir = ctx.boot_dir.join(TLS_DIR);
        let tls = ctx.config.tls_tcp && TLS_FILES.iter().all(|f| tls_dir.join(f).exists());
        let cfg = daemon_json(&ctx.config, tls.then_some(tls_dir.as_path()));
        let path = Self::daemon_json_path(ctx);
        std::fs::create_dir_all(path.parent().unwrap()).ctx(|| format!("mkdir {}", path.display()))?;
        let text = serde_json::to_string_pretty(&cfg).expect("json serialisation");
        std::fs::write(&path, text).ctx(|| format!("write {}", path.display()))?;
        for d in ["/run/containerd", "/etc/docker"] {
            let _ = std::fs::create_dir_all(d);
        }
        Ok(())
    }

    fn services(&self, ctx: &EngineContext) -> Vec<ServiceSpec> {
        let limits = CgroupLimits {
            memory_max: ctx.config.daemon_memory_max.clone(),
            // Daemons get priority over noisy containers for API responsiveness.
            cpu_weight: Some(200),
            pids_max: None,
        };
        let deps: Vec<&str> = ctx.depends_on.iter().map(String::as_str).collect();
        let containerd = ServiceSpec::new(
            "containerd",
            &[
                "/usr/bin/containerd",
                "--address",
                CONTAINERD_SOCKET,
                "--root",
                "/var/lib/containerd",
                "--state",
                "/run/containerd",
                "--log-level",
                "warn",
            ],
        )
        .after(&deps)
        .ready(ReadyCheck::PathExists(CONTAINERD_SOCKET.into()))
        .restart(RestartPolicy::Always)
        .limits(limits.clone())
        // Go runtime: return memory to the OS eagerly, we only have 1 GB.
        .env("GOGC", "50")
        .stop_timeout(Duration::from_secs(15));

        let daemon_json = Self::daemon_json_path(ctx);
        let dockerd = ServiceSpec::new(
            "dockerd",
            &[
                "/usr/bin/dockerd",
                "--config-file",
                daemon_json.to_str().unwrap_or("/run/pios/docker/daemon.json"),
                "--containerd",
                CONTAINERD_SOCKET,
            ],
        )
        .after(&["containerd"])
        .ready(ReadyCheck::HttpUnix { socket: DOCKER_SOCKET.into(), path: "/_ping".into() })
        .restart(RestartPolicy::Always)
        .limits(limits)
        .env("GOGC", "50")
        .shutdown_hook(&[paths::PIOS_BIN, "engine-stop"])
        .stop_timeout(Duration::from_secs(30));

        vec![containerd, dockerd]
    }

    fn ready_service(&self) -> &'static str {
        "dockerd"
    }
}

/// Generate dockerd's daemon.json. `tls_dir` enables the mTLS TCP listener.
pub fn daemon_json(cfg: &EngineConfig, tls_dir: Option<&Path>) -> Value {
    let mut hosts = vec![Value::from(format!("unix://{DOCKER_SOCKET}"))];
    let mut m = Map::new();
    m.insert("data-root".into(), json!("/var/lib/docker"));
    m.insert("exec-root".into(), json!("/run/docker"));
    m.insert("pidfile".into(), json!("/run/docker.pid"));
    m.insert("storage-driver".into(), json!("overlay2"));
    m.insert("exec-opts".into(), json!(["native.cgroupdriver=cgroupfs"]));
    // Binary, size-capped logs: cheaper than json-file and protects the SD card.
    m.insert("log-driver".into(), json!("local"));
    m.insert(
        "log-opts".into(),
        json!({ "max-size": cfg.log_max_size, "max-file": cfg.log_max_file.to_string(), "compress": "false" }),
    );
    m.insert("log-level".into(), json!("warn"));
    // Containers keep running across dockerd restarts/upgrades.
    m.insert("live-restore".into(), json!(true));
    // Kernel NAT instead of one docker-proxy process per published port.
    m.insert("userland-proxy".into(), json!(false));
    m.insert("max-concurrent-downloads".into(), json!(cfg.max_concurrent_downloads));
    m.insert("max-concurrent-uploads".into(), json!(2));
    m.insert("features".into(), json!({ "buildkit": true, "containerd-snapshotter": false }));
    m.insert("default-ulimits".into(), json!({ "nofile": { "Name": "nofile", "Soft": 65536, "Hard": 65536 } }));
    m.insert("iptables".into(), json!(true));
    m.insert("ip-forward".into(), json!(true));
    if let Some(dir) = tls_dir {
        hosts.push(json!("tcp://0.0.0.0:2376"));
        m.insert("tls".into(), json!(true));
        m.insert("tlsverify".into(), json!(true));
        m.insert("tlscacert".into(), json!(format!("{}/ca.pem", dir.display())));
        m.insert("tlscert".into(), json!(format!("{}/server-cert.pem", dir.display())));
        m.insert("tlskey".into(), json!(format!("{}/server-key.pem", dir.display())));
    }
    m.insert("hosts".into(), Value::Array(hosts));
    for (k, v) in &cfg.extra {
        m.insert(k.clone(), serde_json::to_value(v).unwrap_or(Value::Null));
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> EngineContext {
        EngineContext {
            config: EngineConfig::default(),
            data_dir: "/data".into(),
            run_dir: "/run/pios".into(),
            boot_dir: "/boot".into(),
            depends_on: vec!["netd".into(), "ntpd".into()],
        }
    }

    #[test]
    fn daemon_json_defaults() {
        let v = daemon_json(&EngineConfig::default(), None);
        assert_eq!(v["storage-driver"], "overlay2");
        assert_eq!(v["live-restore"], true);
        assert_eq!(v["userland-proxy"], false);
        assert_eq!(v["hosts"], json!(["unix:///var/run/docker.sock"]));
        assert!(v.get("tlsverify").is_none());
    }

    #[test]
    fn daemon_json_tls_and_extra() {
        let mut cfg = EngineConfig::default();
        cfg.extra.insert("registry-mirrors".into(), toml::Value::Array(vec!["https://m.example".into()]));
        cfg.extra.insert("log-level".into(), toml::Value::String("info".into()));
        let v = daemon_json(&cfg, Some(&PathBuf::from("/boot/docker-tls")));
        assert_eq!(v["hosts"].as_array().unwrap().len(), 2);
        assert_eq!(v["tlsverify"], true);
        assert_eq!(v["tlscacert"], "/boot/docker-tls/ca.pem");
        assert_eq!(v["registry-mirrors"], json!(["https://m.example"]));
        assert_eq!(v["log-level"], "info", "user extra overrides defaults");
    }

    #[test]
    fn services_are_wired() {
        let s = DockerEngine.services(&ctx());
        assert_eq!(s[0].name, "containerd");
        assert_eq!(s[0].after, vec!["netd", "ntpd"]);
        assert_eq!(s[1].after, vec!["containerd"]);
        assert!(s[1].shutdown_hook.is_some());
        assert!(matches!(s[1].ready, ReadyCheck::HttpUnix { .. }));
    }
}
