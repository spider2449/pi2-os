//! Core building blocks shared by every PiOS module.

pub mod config;
pub mod error;
pub mod ipc;
pub mod log;
pub mod paths;
pub mod service;
#[cfg(target_os = "linux")]
pub mod sys;

pub use error::{Error, Result};
