//! Networking module. Wire formats (`dhcp`, `sntp`) are pure and host
//! testable; `netd`/`ntpd` are the Linux daemons built on them.

pub mod dhcp;
pub mod sntp;

#[cfg(target_os = "linux")]
mod iface;
#[cfg(target_os = "linux")]
pub mod netd;
#[cfg(target_os = "linux")]
pub mod ntpd;
