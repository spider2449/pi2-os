//! Image store on the data partition:
//!
//! ```text
//! images/tags.json          {"docker.io/library/alpine:latest": "sha256:<id>", ...}
//! images/<id>/config.json   image configuration blob, as pulled
//! images/<id>/meta.json     layers (diff ids, lowest first), size, repo digests
//! layers/<diff id>/         extracted layer, overlayfs whiteout format
//! ```

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use pios_core::error::IoContext;
use pios_core::{Error, Result};

use crate::distribution::{ImageConfigFile, ImageRunConfig};
use crate::reference::{familiar, Reference};
use crate::util::parse_rfc3339;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageMeta {
    /// Layer directory names (diff id hex), lowest first.
    pub layers: Vec<String>,
    /// Uncompressed bytes of all layers.
    pub size: u64,
    pub created: i64,
    /// `docker.io/library/alpine@sha256:...`
    pub repo_digests: Vec<String>,
}

pub struct Image {
    /// "sha256:<hex>"
    pub id: String,
    pub meta: ImageMeta,
    pub config: ImageConfigFile,
    pub tags: Vec<String>,
}

impl Image {
    pub fn run_config(&self) -> ImageRunConfig {
        self.config.config.clone().unwrap_or_default()
    }
}

pub struct Store {
    root: PathBuf,
    /// Serialises tag updates (read-modify-write of tags.json).
    tags_lock: Mutex<()>,
}

fn id_hex(id: &str) -> &str {
    id.strip_prefix("sha256:").unwrap_or(id)
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        for d in ["images", "layers", "tmp"] {
            fs::create_dir_all(root.join(d)).ctx(|| format!("mkdir {}/{d}", root.display()))?;
        }
        // Interrupted downloads.
        if let Ok(rd) = fs::read_dir(root.join("tmp")) {
            for e in rd.flatten() {
                let _ = fs::remove_dir_all(e.path());
            }
        }
        Ok(Self { root: root.to_path_buf(), tags_lock: Mutex::new(()) })
    }

    pub fn layers_dir(&self) -> PathBuf {
        self.root.join("layers")
    }

    pub fn layer_path(&self, diff_hex: &str) -> PathBuf {
        self.root.join("layers").join(diff_hex)
    }

    pub fn has_layer(&self, diff_hex: &str) -> bool {
        self.layer_path(diff_hex).is_dir()
    }

    /// Fresh scratch directory under tmp/ (same filesystem as layers/).
    pub fn tmp_dir(&self) -> Result<PathBuf> {
        let p = self.root.join("tmp").join(crate::util::random_id());
        fs::create_dir_all(&p).ctx(|| format!("mkdir {}", p.display()))?;
        Ok(p)
    }

    /// Move an extracted layer into place; a concurrent pull of the same
    /// layer may have won the race, which is fine.
    pub fn commit_layer(&self, tmp: &Path, diff_hex: &str) -> Result<()> {
        let dst = self.layer_path(diff_hex);
        if dst.exists() {
            let _ = fs::remove_dir_all(tmp);
            return Ok(());
        }
        fs::rename(tmp, &dst).ctx(|| format!("rename {} -> {}", tmp.display(), dst.display()))
    }

    fn tags(&self) -> BTreeMap<String, String> {
        fs::read(self.root.join("images/tags.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save_tags(&self, tags: &BTreeMap<String, String>) -> Result<()> {
        write_atomic(&self.root.join("images/tags.json"), &serde_json::to_vec_pretty(tags).expect("json"))
    }

    pub fn save_image(&self, id: &str, config_blob: &[u8], meta: &ImageMeta) -> Result<()> {
        let dir = self.root.join("images").join(id_hex(id));
        fs::create_dir_all(&dir).ctx(|| format!("mkdir {}", dir.display()))?;
        write_atomic(&dir.join("config.json"), config_blob)?;
        let mut meta = meta.clone();
        if let Some(old) = self.meta(id) {
            for d in old.repo_digests {
                if !meta.repo_digests.contains(&d) {
                    meta.repo_digests.push(d);
                }
            }
        }
        write_atomic(&dir.join("meta.json"), &serde_json::to_vec_pretty(&meta).expect("json"))
    }

    fn meta(&self, id: &str) -> Option<ImageMeta> {
        let b = fs::read(self.root.join("images").join(id_hex(id)).join("meta.json")).ok()?;
        serde_json::from_slice(&b).ok()
    }

    /// Point `tag_key` at `id`; returns the previous image id, if different.
    pub fn tag(&self, tag_key: &str, id: &str) -> Result<Option<String>> {
        let _g = self.tags_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut tags = self.tags();
        let old = tags.insert(tag_key.to_string(), id.to_string());
        self.save_tags(&tags)?;
        Ok(old.filter(|o| o != id))
    }

    pub fn image_ids(&self) -> Vec<String> {
        fs::read_dir(self.root.join("images"))
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().join("meta.json").exists())
                    .map(|e| format!("sha256:{}", e.file_name().to_string_lossy()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn load(&self, id: &str) -> Option<Image> {
        let dir = self.root.join("images").join(id_hex(id));
        let meta = self.meta(id)?;
        let config: ImageConfigFile = serde_json::from_slice(&fs::read(dir.join("config.json")).ok()?).ok()?;
        let id = format!("sha256:{}", id_hex(id));
        let tags = self.tags().into_iter().filter(|(_, v)| *v == id).map(|(k, _)| k).collect();
        Some(Image { id, meta, config, tags })
    }

    /// Find an image by reference (`alpine`, `alpine:3.20`, `x@sha256:..`)
    /// or by (a prefix of) its id.
    pub fn resolve(&self, name: &str) -> Option<Image> {
        let ids = self.image_ids();
        let hex = id_hex(name);
        if hex.len() >= 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let matches: Vec<&String> = ids.iter().filter(|id| id_hex(id).starts_with(hex)).collect();
            if matches.len() == 1 {
                return self.load(matches[0]);
            }
        }
        let r = Reference::parse(name).ok()?;
        if let Some(d) = &r.digest {
            let want = format!("{}@{d}", r.full_repo());
            return ids.iter().filter_map(|id| self.load(id)).find(|i| i.meta.repo_digests.contains(&want));
        }
        let id = self.tags().get(&r.tag_key()?)?.clone();
        self.load(&id)
    }

    /// Untag `name`, or delete by id. Returns Docker's response entries.
    /// `in_use(id)` says whether a container still needs the image.
    pub fn remove(&self, name: &str, force: bool, in_use: &dyn Fn(&str) -> bool) -> Result<Vec<Value>> {
        let img = self.resolve(name).ok_or_else(|| Error::msg(format!("No such image: {name}")))?;
        let _g = self.tags_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut tags = self.tags();
        let mut out = Vec::new();
        let by_tag = Reference::parse(name).ok().and_then(|r| r.tag_key()).filter(|k| tags.get(k) == Some(&img.id));
        let to_untag: Vec<String> = match &by_tag {
            Some(k) => vec![k.clone()],
            None => {
                if img.tags.len() > 1 && !force {
                    return Err(Error::msg(format!(
                        "conflict: unable to delete {} (must be forced) - image is referenced in multiple repositories",
                        &id_hex(&img.id)[..12]
                    )));
                }
                img.tags.clone()
            }
        };
        let remaining = img.tags.iter().filter(|t| !to_untag.contains(t)).count();
        if remaining == 0 && in_use(&img.id) && !force {
            return Err(Error::msg(format!(
                "conflict: unable to delete {} - image is being used by a container",
                if by_tag.is_some() { name.to_string() } else { id_hex(&img.id)[..12].to_string() }
            )));
        }
        for t in &to_untag {
            tags.remove(t);
            out.push(json!({ "Untagged": familiar(t) }));
        }
        self.save_tags(&tags)?;
        if remaining == 0 && !in_use(&img.id) {
            let others: Vec<ImageMeta> =
                self.image_ids().iter().filter(|i| **i != img.id).filter_map(|i| self.meta(i)).collect();
            fs::remove_dir_all(self.root.join("images").join(id_hex(&img.id)))
                .ctx(|| format!("remove image {}", img.id))?;
            out.push(json!({ "Deleted": img.id }));
            for l in &img.meta.layers {
                if !others.iter().any(|m| m.layers.contains(l)) {
                    let _ = fs::remove_dir_all(self.layer_path(l));
                    out.push(json!({ "Deleted": format!("sha256:{l}") }));
                }
            }
        }
        Ok(out)
    }

    /// `GET /images/json` entry.
    pub fn summary(&self, img: &Image) -> Value {
        let tags: Vec<String> = img.tags.iter().map(|t| familiar(t)).collect();
        let digests: Vec<String> = img.meta.repo_digests.iter().map(|d| familiar(d)).collect();
        json!({
            "Id": img.id, "ParentId": "", "RepoTags": tags, "RepoDigests": digests,
            "Created": img.meta.created / 1_000_000_000, "Size": img.meta.size, "SharedSize": -1,
            "Labels": img.run_config().labels.unwrap_or_default(), "Containers": -1,
        })
    }

    /// `GET /images/{name}/json`.
    pub fn inspect(&self, img: &Image) -> Value {
        let tags: Vec<String> = img.tags.iter().map(|t| familiar(t)).collect();
        let digests: Vec<String> = img.meta.repo_digests.iter().map(|d| familiar(d)).collect();
        json!({
            "Id": img.id, "RepoTags": tags, "RepoDigests": digests, "Parent": "", "Comment": "",
            "Created": img.config.created.clone().unwrap_or_default(), "Container": "",
            "Config": img.run_config(), "Architecture": img.config.architecture, "Variant": img.config.variant,
            "Os": img.config.os, "Size": img.meta.size, "VirtualSize": img.meta.size,
            "GraphDriver": {"Name": "overlay2", "Data": {}},
            "RootFS": {"Type": "layers", "Layers": img.meta.layers.iter().map(|l| format!("sha256:{l}")).collect::<Vec<_>>()},
            "Metadata": {"LastTagTime": "0001-01-01T00:00:00Z"},
        })
    }

    /// Image creation time from its configuration.
    pub fn created_ns(config: &ImageConfigFile) -> i64 {
        config.created.as_deref().and_then(parse_rfc3339).unwrap_or(0)
    }
}

pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).ctx(|| format!("create {}", tmp.display()))?;
        io::Write::write_all(&mut f, data).ctx(|| format!("write {}", tmp.display()))?;
        f.sync_all().ctx(|| format!("fsync {}", tmp.display()))?;
    }
    fs::rename(&tmp, path).ctx(|| format!("rename {}", path.display()))
}

fn cpath(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("path contains NUL")
}

/// Unpack one layer tarball into `dest`, translating OCI whiteouts into
/// overlayfs ones: `.wh.<name>` becomes a 0:0 character device and
/// `.wh..wh..opq` the `trusted.overlay.opaque` xattr. Returns bytes written.
pub fn extract_layer(input: impl Read, dest: &Path) -> Result<u64> {
    let mut archive = tar::Archive::new(input);
    archive.set_preserve_permissions(true);
    // SAFETY: plain getter. Only root can chown (always the case on the board).
    archive.set_preserve_ownerships(unsafe { libc::geteuid() } == 0);
    archive.set_preserve_mtime(true);
    archive.set_unpack_xattrs(true);
    archive.set_overwrite(true);
    let mut total = 0u64;
    let tar_err = |e: io::Error| Error::msg(format!("layer: {e}"));
    for entry in archive.entries().map_err(tar_err)? {
        let mut entry = entry.map_err(tar_err)?;
        let path = entry.path().map_err(tar_err)?.into_owned();
        // Normalise and refuse anything that climbs out of the layer.
        let mut rel = PathBuf::new();
        for c in path.components() {
            match c {
                Component::Normal(p) => rel.push(p),
                Component::CurDir | Component::RootDir => {}
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(Error::msg(format!("layer entry escapes the root: {}", path.display())));
                }
            }
        }
        let Some(name) = rel.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
        if let Some(target) = name.strip_prefix(".wh.") {
            let parent = dest.join(rel.parent().unwrap_or(Path::new("")));
            fs::create_dir_all(&parent).ctx(|| format!("mkdir {}", parent.display()))?;
            if target == ".wh..opq" {
                let p = cpath(&parent);
                // SAFETY: valid C strings and buffer for the call.
                let r = unsafe { libc::setxattr(p.as_ptr(), c"trusted.overlay.opaque".as_ptr(), b"y".as_ptr().cast(), 1, 0) };
                if r < 0 {
                    return Err(Error::msg(format!("opaque whiteout on {}: {}", parent.display(), io::Error::last_os_error())));
                }
            } else {
                let w = parent.join(target);
                let _ = fs::remove_dir_all(&w);
                let _ = fs::remove_file(&w);
                let p = cpath(&w);
                // SAFETY: valid C string; 0:0 char device is overlayfs' whiteout.
                if unsafe { libc::mknod(p.as_ptr(), libc::S_IFCHR, 0) } < 0 {
                    return Err(Error::msg(format!("whiteout {}: {}", w.display(), io::Error::last_os_error())));
                }
            }
            continue;
        }
        total += entry.size();
        entry.unpack_in(dest).map_err(|e| Error::msg(format!("unpack {}: {e}", rel.display())))?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pios-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn layer(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut h = tar::Header::new_ustar();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            b.append_data(&mut h, name, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn tags_resolve_and_remove() {
        let root = tmp("tags");
        let s = Store::open(&root).unwrap();
        let id = format!("sha256:{}", "ab".repeat(32));
        let cfg = br#"{"architecture":"arm","os":"linux","config":{"Cmd":["sh"]},"rootfs":{"diff_ids":[]}}"#;
        fs::create_dir_all(s.layer_path("l1")).unwrap();
        s.save_image(&id, cfg, &ImageMeta { layers: vec!["l1".into()], size: 5, created: 0, repo_digests: vec![] }).unwrap();
        s.tag("docker.io/library/alpine:latest", &id).unwrap();
        s.tag("docker.io/library/alpine:3", &id).unwrap();
        assert_eq!(s.resolve("alpine").unwrap().id, id);
        assert_eq!(s.resolve("abababab").unwrap().id, id);
        assert_eq!(s.resolve("alpine:3").unwrap().run_config().cmd.unwrap(), vec!["sh"]);
        assert!(s.resolve("nginx").is_none());
        assert_eq!(s.summary(&s.resolve("alpine").unwrap())["RepoTags"].as_array().unwrap().len(), 2);
        // Removing by id with two tags needs force; by tag only untags.
        assert!(s.remove(&id, false, &|_| false).is_err());
        let r = s.remove("alpine:3", false, &|_| false).unwrap();
        assert_eq!(r, vec![json!({"Untagged": "alpine:3"})]);
        assert!(s.remove("alpine", false, &|_| true).is_err(), "in use by a container");
        let r = s.remove("alpine", false, &|_| false).unwrap();
        assert_eq!(r.len(), 3);
        assert!(!s.layer_path("l1").exists());
        assert!(s.image_ids().is_empty());
    }

    #[test]
    fn extracts_with_whiteouts() {
        if unsafe { libc::geteuid() } != 0 {
            // Whiteouts need mknod/trusted xattrs: covered on the board.
            return;
        }
        let d = tmp("wh");
        extract_layer(&layer(&[("etc/a", b"1"), ("etc/.wh.b", b""), ("var/.wh..wh..opq", b"")])[..], &d).unwrap();
        assert_eq!(fs::read(d.join("etc/a")).unwrap(), b"1");
        use std::os::unix::fs::FileTypeExt;
        assert!(fs::symlink_metadata(d.join("etc/b")).unwrap().file_type().is_char_device());
    }

    #[test]
    fn refuses_escaping_entries() {
        let d = tmp("esc");
        let mut raw = layer(&[("x", b"1")]);
        // Rewrite the name field in place to "../x" (checksum recomputed).
        raw[..4].copy_from_slice(b"../x");
        let mut h = tar::Header::from_byte_slice(&raw[..512]).clone();
        h.set_cksum();
        raw[..512].copy_from_slice(h.as_bytes());
        assert!(extract_layer(&raw[..], &d).is_err());
        assert!(fs::read_dir(&d).unwrap().next().is_none());
        let ok = layer(&[("plain", b"data")]);
        assert_eq!(extract_layer(&ok[..], &d).unwrap(), 4);
    }
}
