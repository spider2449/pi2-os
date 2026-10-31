//! Files for the optional dropbear SSH server. busybox applet links for the
//! shell are created when the rootfs is built (rootfs/busybox-applets.txt).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use pios_core::config::SshConfig;
use pios_core::error::IoContext;
use pios_core::{info, paths, warn, Result};

/// Prepare root's home, shadow entry, authorized keys and the persistent
/// host key.
pub fn prepare_ssh(cfg: &SshConfig) -> Result<()> {
    if !cfg.enabled {
        return Ok(());
    }
    let ssh_dir = Path::new(paths::ROOT_HOME).join(".ssh");
    std::fs::create_dir_all(&ssh_dir).ctx(|| format!("mkdir {}", ssh_dir.display()))?;
    set_mode(paths::ROOT_HOME, 0o700)?;
    set_mode(&ssh_dir, 0o700)?;
    write_shadow(cfg.password_hash.as_deref())?;
    // dropbear rejects keys with loose permissions, and FAT has no real
    // permissions, so copy instead of linking.
    let target = ssh_dir.join("authorized_keys");
    match std::fs::copy(paths::AUTHORIZED_KEYS, &target) {
        Ok(_) => set_mode(&target, 0o600)?,
        Err(_) if cfg.password_hash.is_some() => {}
        Err(_) => warn!("shell", "ssh enabled but {} is missing: nobody can log in", paths::AUTHORIZED_KEYS),
    }
    if !Path::new(paths::SSH_HOST_KEY).exists() {
        if let Some(dir) = Path::new(paths::SSH_HOST_KEY).parent() {
            std::fs::create_dir_all(dir).ctx(|| format!("mkdir {}", dir.display()))?;
        }
        info!("shell", "generating SSH host key");
        let status = Command::new("/usr/bin/dropbearkey")
            .args(["-t", "ed25519", "-f", paths::SSH_HOST_KEY])
            .stdout(std::process::Stdio::null())
            .status()
            .ctx(|| "dropbearkey".into())?;
        if !status.success() {
            warn!("shell", "dropbearkey failed: {status}");
        }
    }
    Ok(())
}

/// /etc/shadow links into tmpfs because the rootfs is read-only. Without a
/// hash root's password is locked ('!'), so only keys work.
fn write_shadow(hash: Option<&str>) -> Result<()> {
    std::fs::write(paths::SHADOW, shadow_entry(hash)).ctx(|| format!("write {}", paths::SHADOW))?;
    set_mode(paths::SHADOW, 0o600)
}

fn shadow_entry(hash: Option<&str>) -> String {
    format!("root:{}:0:0:99999:7:::\n", hash.unwrap_or("!"))
}

fn set_mode(path: impl AsRef<Path>, mode: u32) -> Result<()> {
    let path = path.as_ref();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).ctx(|| format!("chmod {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_locks_root_without_a_hash() {
        assert_eq!(shadow_entry(None), "root:!:0:0:99999:7:::\n");
        assert_eq!(shadow_entry(Some("$6$s$h")), "root:$6$s$h:0:0:99999:7:::\n");
    }
}
