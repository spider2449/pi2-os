//! Well-known filesystem locations. Kept in one place so modules agree.

/// FAT boot partition (firmware, kernel, user configuration).
pub const BOOT_DIR: &str = "/boot";
/// User configuration file on the boot partition.
pub const CONFIG_FILE: &str = "/boot/pios.toml";
/// Persistent ext4 data partition.
pub const DATA_DIR: &str = "/data";
/// Volatile runtime state owned by PiOS.
pub const RUN_DIR: &str = "/run/pios";
/// Control socket served by PID 1.
pub const CTL_SOCKET: &str = "/run/pios/ctl.sock";
/// Marker written by netd once an address is configured.
pub const NET_READY: &str = "/run/pios/net-ready";
/// Marker written by ntpd after the first successful clock sync.
pub const TIME_SYNCED: &str = "/run/pios/time-synced";
/// Generated resolver configuration (/etc/resolv.conf links here).
pub const RESOLV_CONF: &str = "/run/pios/resolv.conf";
/// Last known wall clock, restored early on boot (Pi 2 has no RTC).
pub const SAVED_CLOCK: &str = "/data/pios/clock";
/// Path of the multi-call binary.
pub const PIOS_BIN: &str = "/sbin/pios";
