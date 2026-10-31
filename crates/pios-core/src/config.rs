//! User configuration (`/boot/pios.toml`). Every field has a sane default so
//! an empty or missing file boots a working system.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PiosConfig {
    pub hostname: String,
    /// Log verbosity: "error", "warn", "info" or "debug".
    pub log_level: String,
    /// Mirror service output to the serial console.
    pub console_service_logs: bool,
    pub network: NetworkConfig,
    pub ntp: NtpConfig,
    pub storage: StorageConfig,
    pub tune: TuneConfig,
    pub engine: EngineConfig,
    pub remote: RemoteConfig,
    pub ssh: SshConfig,
}

impl Default for PiosConfig {
    fn default() -> Self {
        Self {
            hostname: "pios".into(),
            log_level: "info".into(),
            console_service_logs: false,
            network: NetworkConfig::default(),
            ntp: NtpConfig::default(),
            storage: StorageConfig::default(),
            tune: TuneConfig::default(),
            engine: EngineConfig::default(),
            remote: RemoteConfig::default(),
            ssh: SshConfig::default(),
        }
    }
}

impl PiosConfig {
    pub fn parse(text: &str) -> Result<Self> {
        let cfg: PiosConfig = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load the file, falling back to defaults if it does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Io { context: format!("read {}", path.display()), source: e }),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.hostname.is_empty() || self.hostname.len() > 63 {
            return Err(Error::Config("hostname must be 1..=63 characters".into()));
        }
        if let NetworkMode::Static = self.network.mode {
            let s = self
                .network
                .static_ip
                .as_ref()
                .ok_or_else(|| Error::Config("network.mode = \"static\" requires [network.static_ip]".into()))?;
            if s.prefix > 32 {
                return Err(Error::Config("network.static_ip.prefix must be <= 32".into()));
            }
        }
        if !(0.0..=1.0).contains(&self.storage.zram_fraction) {
            return Err(Error::Config("storage.zram_fraction must be within 0.0..=1.0".into()));
        }
        if let Some(h) = &self.ssh.password_hash {
            // Only crypt hashes ($id$salt$hash): a plain-text password would
            // silently never match, and ':' or newlines would corrupt /etc/shadow.
            if !h.starts_with('$') || h.len() < 8 || h.contains([':', '\n', '\r']) {
                return Err(Error::Config(
                    "ssh.password_hash must be a crypt hash such as $6$... (openssl passwd -6)".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    Dhcp,
    Static,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    /// Uplink name, or "auto" for the first physical interface (eth0 on the
    /// Pi 2, usb0 under QEMU).
    pub interface: String,
    pub mode: NetworkMode,
    pub static_ip: Option<StaticIp>,
    /// Extra DNS servers, appended after the ones learned via DHCP.
    pub dns: Vec<std::net::Ipv4Addr>,
    /// Seconds to wait for link carrier before retrying.
    pub carrier_timeout_secs: u64,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            interface: "auto".into(),
            mode: NetworkMode::Dhcp,
            static_ip: None,
            dns: Vec::new(),
            carrier_timeout_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StaticIp {
    pub address: std::net::Ipv4Addr,
    pub prefix: u8,
    pub gateway: Option<std::net::Ipv4Addr>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct NtpConfig {
    pub enabled: bool,
    pub servers: Vec<String>,
    /// Resync interval in seconds.
    pub interval_secs: u64,
}

impl Default for NtpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            servers: vec!["pool.ntp.org".into(), "time.cloudflare.com".into()],
            interval_secs: 3600,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Whole SD card block device.
    pub disk: String,
    /// Data partition number on `disk` (always the last partition).
    pub data_partition: u32,
    /// Grow the data partition to the end of the disk on boot.
    pub grow_data: bool,
    /// ext4 mount options for the data partition.
    pub data_mount_options: String,
    /// Fraction of RAM used as compressed zram swap (0 disables).
    pub zram_fraction: f64,
    /// zram compression algorithm (lz4 is the fastest on Cortex-A7).
    pub zram_algorithm: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            disk: "/dev/mmcblk0".into(),
            data_partition: 4,
            grow_data: true,
            data_mount_options: "noatime,commit=60".into(),
            zram_fraction: 0.25,
            zram_algorithm: "lz4".into(),
        }
    }
}

impl StorageConfig {
    /// Device node of the data partition, e.g. `/dev/mmcblk0p4`.
    pub fn data_device(&self) -> String {
        partition_device(&self.disk, self.data_partition)
    }
}

/// Linux partition node naming: disks ending in a digit get a `p` separator.
pub fn partition_device(disk: &str, n: u32) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{disk}p{n}")
    } else {
        format!("{disk}{n}")
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TuneConfig {
    pub cpu_governor: String,
    /// I/O scheduler for the SD card queue.
    pub io_scheduler: String,
    pub read_ahead_kb: u32,
    /// Extra or overriding sysctls, e.g. `"vm.swappiness" = "80"`.
    pub sysctl: BTreeMap<String, String>,
}

impl Default for TuneConfig {
    fn default() -> Self {
        Self {
            cpu_governor: "performance".into(),
            io_scheduler: "mq-deadline".into(),
            read_ahead_kb: 512,
            sysctl: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Docker,
    Native,
    None,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    pub kind: EngineKind,
    /// Block engine start until the clock is synced (image pulls need valid TLS time).
    pub wait_time_sync: bool,
    /// Expose the engine API on TCP 2376 with mutual TLS when certificates exist.
    pub tls_tcp: bool,
    /// Default OCI runtime of the docker engine. "runc" is built in; any other name is registered
    /// from /usr/bin/<name> (crun was evaluated and rejected: slower on this
    /// board and it ignored --pid=host).
    pub runtime: String,
    pub max_concurrent_downloads: u32,
    pub log_max_size: String,
    pub log_max_file: u32,
    /// Hard memory cap for the engine daemons (cgroup v2 memory.max), e.g. "256M".
    pub daemon_memory_max: Option<String>,
    /// Extra keys merged verbatim into the engine's daemon configuration.
    pub extra: BTreeMap<String, toml::Value>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            kind: EngineKind::Docker,
            wait_time_sync: true,
            tls_tcp: true,
            runtime: "runc".into(),
            max_concurrent_downloads: 2,
            log_max_size: "10m".into(),
            log_max_file: 3,
            daemon_memory_max: None,
            extra: BTreeMap::new(),
        }
    }
}

/// Remote management: the pios-ctl protocol over mutual TLS, independent of
/// the container engine so the board stays manageable if dockerd is down.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteConfig {
    /// Serve when the certificates in /boot/docker-tls exist.
    pub enabled: bool,
    pub port: u16,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self { enabled: true, port: 2377 }
    }
}

/// Optional SSH server (dropbear). Public keys always work; root password
/// logins are allowed only when a password hash is configured.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SshConfig {
    /// Off by default: one less daemon and attack surface. Needs
    /// /boot/authorized_keys or `password_hash` to be useful.
    pub enabled: bool,
    pub port: u16,
    /// crypt(3) hash of root's password (`openssl passwd -6`, or busybox
    /// `mkpasswd -m sha512`). Unset = key-only logins.
    pub password_hash: Option<String>,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self { enabled: false, port: 22, password_hash: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(PiosConfig::parse("").unwrap(), PiosConfig::default());
    }

    #[test]
    fn parses_full_example() {
        let cfg = PiosConfig::parse(
            r#"
            hostname = "edge-01"
            [network]
            mode = "static"
            static_ip = { address = "192.168.1.50", prefix = 24, gateway = "192.168.1.1" }
            dns = ["1.1.1.1"]
            [storage]
            zram_fraction = 0.5
            [tune.sysctl]
            "vm.swappiness" = "80"
            [engine]
            kind = "docker"
            daemon_memory_max = "256M"
            [engine.extra]
            "registry-mirrors" = ["https://mirror.example"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.hostname, "edge-01");
        assert_eq!(cfg.network.mode, NetworkMode::Static);
        assert_eq!(cfg.network.static_ip.unwrap().prefix, 24);
        assert_eq!(cfg.tune.sysctl["vm.swappiness"], "80");
        assert!(cfg.engine.extra.contains_key("registry-mirrors"));
    }

    #[test]
    fn shipped_example_matches_defaults() {
        let text = include_str!("../../../rootfs/overlay/etc/pios/pios.toml.example");
        assert_eq!(PiosConfig::parse(text).unwrap(), PiosConfig::default());
    }

    #[test]
    fn static_mode_requires_address() {
        assert!(PiosConfig::parse("[network]\nmode = \"static\"").is_err());
    }

    #[test]
    fn ssh_password_hash_must_be_a_crypt_hash() {
        assert!(PiosConfig::parse("[ssh]\npassword_hash = \"hunter2\"").is_err());
        assert!(PiosConfig::parse("[ssh]\npassword_hash = \"$6$salt$a:b\"").is_err());
        let cfg = PiosConfig::parse("[ssh]\npassword_hash = \"$6$salt$hashhash\"").unwrap();
        assert_eq!(cfg.ssh.password_hash.as_deref(), Some("$6$salt$hashhash"));
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(PiosConfig::parse("hostnme = \"typo\"").is_err());
    }

    #[test]
    fn partition_naming() {
        assert_eq!(partition_device("/dev/mmcblk0", 4), "/dev/mmcblk0p4");
        assert_eq!(partition_device("/dev/sda", 2), "/dev/sda2");
    }
}
