//! Interface every container engine module implements. PID 1 only knows
//! this trait; switching engines is a config change (`[engine] kind`).

use std::path::PathBuf;

use pios_core::config::EngineConfig;
use pios_core::service::ServiceSpec;
use pios_core::Result;

/// Environment handed to the engine.
#[derive(Debug, Clone)]
pub struct EngineContext {
    pub config: EngineConfig,
    /// Persistent storage root (data partition).
    pub data_dir: PathBuf,
    /// Volatile per-boot directory for generated files.
    pub run_dir: PathBuf,
    /// Boot partition, holds user supplied material such as TLS certificates.
    pub boot_dir: PathBuf,
    /// Services the engine must wait for (network, time sync).
    pub depends_on: Vec<String>,
}

pub trait ContainerEngine {
    /// Short identifier used in logs and service names.
    fn name(&self) -> &'static str;

    /// Write configuration and create directories. Runs once at boot,
    /// before any service starts.
    fn prepare(&self, ctx: &EngineContext) -> Result<()>;

    /// Daemons to supervise, in any order (dependencies are explicit).
    fn services(&self, ctx: &EngineContext) -> Vec<ServiceSpec>;

    /// Name of the service whose readiness means "engine is usable".
    fn ready_service(&self) -> &'static str;
}
