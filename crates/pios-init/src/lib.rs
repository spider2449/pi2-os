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
        assert_eq!(s[1].after, vec!["netd"]);
    }

    #[test]
    fn engine_waits_for_time_only_when_enabled() {
        let mut cfg = PiosConfig::default();
        assert_eq!(engine_dependencies(&cfg), vec!["netd", "ntpd"]);
        cfg.engine.wait_time_sync = false;
        assert_eq!(engine_dependencies(&cfg), vec!["netd"]);
    }
}
