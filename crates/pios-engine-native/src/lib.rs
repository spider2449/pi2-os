//! Placeholder for the Rust-native engine (youki OCI runtime + a lean daemon
//! serving a subset of the Docker Engine API). Selecting it today fails
//! loudly at boot instead of silently running nothing.

use pios_core::service::ServiceSpec;
use pios_core::{Error, Result};
use pios_engine_api::{ContainerEngine, EngineContext};

pub struct NativeEngine;

impl ContainerEngine for NativeEngine {
    fn name(&self) -> &'static str {
        "native"
    }

    fn prepare(&self, _ctx: &EngineContext) -> Result<()> {
        Err(Error::msg("native engine is not implemented yet; set [engine] kind = \"docker\""))
    }

    fn services(&self, _ctx: &EngineContext) -> Vec<ServiceSpec> {
        Vec::new()
    }

    fn ready_service(&self) -> &'static str {
        "native"
    }
}
