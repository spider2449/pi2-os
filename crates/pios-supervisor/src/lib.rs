//! Process supervisor run inside PID 1.
//!
//! Pure scheduling logic (`graph`, `backoff`, `ring`) is platform independent
//! and unit tested on the host; the runtime (`runtime`, `cgroup`, `ready`)
//! drives real processes on Linux.

pub mod backoff;
pub mod graph;
pub mod ring;

#[cfg(target_os = "linux")]
mod cgroup;
#[cfg(target_os = "linux")]
mod ready;
#[cfg(target_os = "linux")]
mod runtime;

#[cfg(target_os = "linux")]
pub use runtime::{Exit, Supervisor};
