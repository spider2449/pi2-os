//! Storage module: pure on-disk formats (`mbr`, `swap`) plus the Linux
//! boot-time setup that uses them.

pub mod mbr;
pub mod swap;

#[cfg(target_os = "linux")]
mod setup;
#[cfg(target_os = "linux")]
pub use setup::{setup_data, setup_zram, PERSISTENT};
