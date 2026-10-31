//! PID 1. Boot order:
//! early mounts → boot partition + config → clock floor → data partition →
//! zram → tuning → services (netd, ntpd, engine) under the supervisor →
//! orderly shutdown on signal or `pios-ctl reboot|poweroff`.

use pios_core::config::{EngineKind, NetworkMode, PiosConfig};
use pios_core::service::{ReadyCheck, RestartPolicy, ServiceSpec};
use pios_engine_api::ContainerEngine;

/// Factory supplied by the binary so only the compiled-in engines exist.
pub type EngineSelector = dyn Fn(EngineKind) -> Option<Box<dyn ContainerEngine>>;

/// Default boot partition; override with `pios.boot=<dev>` on the kernel cmdline.
pub const DEFAULT_BOOT_DEVICE: &str = "/dev/mmcblk0p1";

/// Value of `key=` on the kernel command line.
pub fn cmdline_value<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_whitespace().find_map(|arg| arg.strip_prefix(key)?.strip_prefix('='))
}

/// The system services every configuration runs, before the engine's.
pub fn system_services(cfg: &PiosConfig) -> Vec<ServiceSpec> {
    let bin = pios_core::paths::PIOS_BIN;
    let mut v = Vec::new();
    let netd = ServiceSpec::new("netd", &[bin, "netd"]);
    v.push(match cfg.network.mode {
        NetworkMode::Static => netd.ready(ReadyCheck::Exited).restart(RestartPolicy::OnFailure),
        NetworkMode::Dhcp => netd.ready(ReadyCheck::PathExists(pios_core::paths::NET_READY.into())),
    });
    if cfg.remote.enabled {
        // Independent of the engine on purpose: the rescue path when it is down.
        // Exits 0 without certificates, which marks it done.
        v.push(ServiceSpec::new("ctld", &[bin, "ctld"]).after(&["netd"]).restart(RestartPolicy::OnFailure));
    }
    if cfg.ssh.enabled {
        let port = cfg.ssh.port.to_string();
        v.push(
            // -F foreground, -E log to stderr, -s/-g no password logins at all.
            ServiceSpec::new(
                "sshd",
                &["/usr/sbin/dropbear", "-F", "-E", "-s", "-g", "-p", &port, "-r", pios_core::paths::SSH_HOST_KEY],
            )
            .after(&["netd"]),
        );
    }
    if cfg.ntp.enabled {
        v.push(
            ServiceSpec::new("ntpd", &[bin, "ntpd"])
                .after(&["netd"])
                .ready(ReadyCheck::PathExists(pios_core::paths::TIME_SYNCED.into())),
        );
    }
    v
}

/// Services the engine must wait for.
pub fn engine_dependencies(cfg: &PiosConfig) -> Vec<String> {
    let mut deps = vec!["netd".to_string()];
    if cfg.ntp.enabled && cfg.engine.wait_time_sync {
        deps.push("ntpd".into());
    }
    deps
}

#[cfg(target_os = "linux")]
mod boot;
#[cfg(target_os = "linux")]
mod status;
#[cfg(target_os = "linux")]
mod shell;
#[cfg(target_os = "linux")]
pub use boot::run;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cmdline() {
        let c = "console=serial0,115200 root=/dev/mmcblk0p2 pios.boot=/dev/sda1 quiet";
        assert_eq!(cmdline_value(c, "pios.boot"), Some("/dev/sda1"));
        assert_eq!(cmdline_value(c, "root"), Some("/dev/mmcblk0p2"));
        assert_eq!(cmdline_value(c, "missing"), None);
        assert_eq!(cmdline_value("rootwait", "root"), None);
    }

    #[test]
    fn static_network_is_oneshot() {
        let cfg = PiosConfig::parse(
            "[network]\nmode = \"static\"\nstatic_ip = { address = \"10.0.0.2\", prefix = 24 }",
        )
        .unwrap();
        let s = system_services(&cfg);
        assert_eq!(s[0].ready, ReadyCheck::Exited);
        assert!(s[1..].iter().all(|x| x.after == vec!["netd"]));
    }

    #[test]
    fn ssh_is_opt_in_and_key_only() {
        assert!(!system_services(&PiosConfig::default()).iter().any(|x| x.name == "sshd"));
        let cfg = PiosConfig::parse("[ssh]\nenabled = true\nport = 2222").unwrap();
        let s = system_services(&cfg);
        let sshd = s.iter().find(|x| x.name == "sshd").unwrap();
        assert!(sshd.exec.contains(&"-s".to_string()) && sshd.exec.contains(&"-g".to_string()));
        assert!(sshd.exec.windows(2).any(|w| w == ["-p", "2222"]));
        assert_eq!(sshd.after, vec!["netd"]);
    }

    #[test]
    fn remote_management_is_independent_of_engine() {
        let s = system_services(&PiosConfig::default());
        let ctld = s.iter().find(|x| x.name == "ctld").unwrap();
        assert_eq!(ctld.after, vec!["netd"]);
        let mut cfg = PiosConfig::default();
        cfg.remote.enabled = false;
        assert!(!system_services(&cfg).iter().any(|x| x.name == "ctld"));
    }

    #[test]
    fn engine_waits_for_time_only_when_enabled() {
        let mut cfg = PiosConfig::default();
        assert_eq!(engine_dependencies(&cfg), vec!["netd", "ntpd"]);
        cfg.engine.wait_time_sync = false;
        assert_eq!(engine_dependencies(&cfg), vec!["netd"]);
    }
}
