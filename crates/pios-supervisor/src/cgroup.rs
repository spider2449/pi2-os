//! cgroup v2 placement: every service gets `/sys/fs/cgroup/pios/<name>`.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use pios_core::error::IoContext;
use pios_core::service::CgroupLimits;
use pios_core::sys::write_attr;
use pios_core::{warn, Result};

pub const ROOT: &str = "/sys/fs/cgroup";
const PARENT: &str = "/sys/fs/cgroup/pios";
const CONTROLLERS: &str = "+cpu +cpuset +memory +io +pids";

/// Enable controllers for the subtree that holds service cgroups.
pub fn init() -> Result<()> {
    for dir in [ROOT, PARENT] {
        std::fs::create_dir_all(dir).ctx(|| format!("mkdir {dir}"))?;
        // Enable controllers one by one so a missing one does not block the rest.
        for c in CONTROLLERS.split(' ') {
            if let Err(e) = write_attr(format!("{dir}/cgroup.subtree_control"), c) {
                warn!("cgroup", "{e}");
            }
        }
    }
    Ok(())
}

/// An opened `cgroup.procs` handle; the child writes "0" into it right
/// before exec, which moves itself into the cgroup without any allocation.
pub struct Cgroup {
    procs: File,
}

impl Cgroup {
    pub fn create(service: &str, limits: &CgroupLimits) -> Result<Self> {
        let dir = PathBuf::from(PARENT).join(service);
        std::fs::create_dir_all(&dir).ctx(|| format!("mkdir {}", dir.display()))?;
        if let Some(m) = &limits.memory_max {
            write_attr(dir.join("memory.max"), m)?;
        }
        if let Some(w) = limits.cpu_weight {
            write_attr(dir.join("cpu.weight"), &w.to_string())?;
        }
        if let Some(p) = limits.pids_max {
            write_attr(dir.join("pids.max"), &p.to_string())?;
        }
        let procs = OpenOptions::new()
            .write(true)
            .open(dir.join("cgroup.procs"))
            .ctx(|| format!("open {}/cgroup.procs", dir.display()))?;
        Ok(Self { procs })
    }

    pub fn procs_fd(&self) -> i32 {
        self.procs.as_raw_fd()
    }
}
