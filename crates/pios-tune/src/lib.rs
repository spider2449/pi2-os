//! Performance tuning applied once at boot.

use std::collections::BTreeMap;

use pios_core::config::{StorageConfig, TuneConfig};

/// Defaults chosen for a 1 GB, 4-core Cortex-A7 board running containers
/// from an SD card. User entries in `[tune.sysctl]` override these.
pub const DEFAULT_SYSCTLS: &[(&str, &str)] = &[
    // Container networking.
    ("net.ipv4.ip_forward", "1"),
    ("net.ipv6.conf.all.forwarding", "1"),
    ("net.bridge.bridge-nf-call-iptables", "1"),
    ("net.bridge.bridge-nf-call-ip6tables", "1"),
    ("net.core.somaxconn", "4096"),
    ("net.core.netdev_max_backlog", "2048"),
    ("net.ipv4.tcp_fastopen", "3"),
    ("net.ipv4.tcp_slow_start_after_idle", "0"),
    // Start even / end odd, as the kernel recommends for port selection.
    ("net.ipv4.ip_local_port_range", "10240 65001"),
    ("net.netfilter.nf_conntrack_max", "65536"),
    // Memory: zram swap is cheap, SD writeback should be batched.
    ("vm.swappiness", "100"),
    ("vm.page-cluster", "0"),
    ("vm.vfs_cache_pressure", "50"),
    ("vm.dirty_background_ratio", "5"),
    ("vm.dirty_ratio", "20"),
    ("vm.dirty_writeback_centisecs", "1500"),
    ("vm.overcommit_memory", "1"),
    ("vm.min_free_kbytes", "16384"),
    // Limits containers commonly hit.
    ("fs.inotify.max_user_watches", "524288"),
    ("fs.inotify.max_user_instances", "512"),
    ("fs.file-max", "1048576"),
    ("kernel.pid_max", "65536"),
    ("kernel.keys.maxkeys", "2000"),
    // Reboot automatically on panic or oops instead of hanging headless.
    ("kernel.panic", "5"),
    ("kernel.panic_on_oops", "1"),
];

/// Effective sysctl set: defaults overridden/extended by the user.
pub fn effective_sysctls(user: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut m: BTreeMap<String, String> =
        DEFAULT_SYSCTLS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    m.extend(user.iter().map(|(k, v)| (k.clone(), v.clone())));
    m
}

/// `/proc/sys` path for a dotted sysctl key.
pub fn sysctl_path(key: &str) -> String {
    format!("/proc/sys/{}", key.replace('.', "/"))
}

#[cfg(target_os = "linux")]
pub fn apply(tune: &TuneConfig, storage: &StorageConfig) {
    use pios_core::sys::write_attr;
    use pios_core::{debug, warn};

    for (k, v) in effective_sysctls(&tune.sysctl) {
        // Some keys only exist once a module (bridge, conntrack) is active.
        if let Err(e) = write_attr(sysctl_path(&k), &v) {
            debug!("tune", "{e}");
        }
    }
    if let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") {
        for e in entries.flatten() {
            let gov = e.path().join("cpufreq/scaling_governor");
            if gov.exists() {
                if let Err(e) = write_attr(&gov, &tune.cpu_governor) {
                    warn!("tune", "{e}");
                }
            }
        }
    }
    let disk = std::path::Path::new(&storage.disk).file_name().and_then(|n| n.to_str()).unwrap_or("mmcblk0");
    let q = format!("/sys/block/{disk}/queue");
    if let Err(e) = write_attr(format!("{q}/scheduler"), &tune.io_scheduler) {
        warn!("tune", "{e}");
    }
    let _ = write_attr(format!("{q}/read_ahead_kb"), &tune.read_ahead_kb.to_string());
}

#[cfg(not(target_os = "linux"))]
pub fn apply(_tune: &TuneConfig, _storage: &StorageConfig) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_overrides_defaults() {
        let user = BTreeMap::from([("vm.swappiness".to_string(), "60".to_string()), ("x.y".into(), "1".into())]);
        let eff = effective_sysctls(&user);
        assert_eq!(eff["vm.swappiness"], "60");
        assert_eq!(eff["x.y"], "1");
        assert_eq!(eff["net.ipv4.ip_forward"], "1");
    }

    #[test]
    fn path_mapping() {
        assert_eq!(sysctl_path("net.ipv4.ip_forward"), "/proc/sys/net/ipv4/ip_forward");
    }
}
