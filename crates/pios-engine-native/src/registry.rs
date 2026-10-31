//! Image pulls: OCI distribution API with token auth, linux/arm/v7
//! selection, layers streamed through sha256 + gunzip straight into place.

use std::io::{BufReader, Read};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use flate2::read::GzDecoder;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use pios_core::{info, Error, Result};

use crate::client::{Client, Response};
use crate::distribution::*;
use crate::reference::Reference;
use crate::store::{extract_layer, ImageMeta, Store};
use crate::util::{base64_encode, hex};

const MAX_MANIFEST: u64 = 4 << 20;

/// Credentials from the client's X-Registry-Auth header.
#[derive(Debug, Clone, Default)]
pub struct Auth {
    pub username: String,
    pub password: String,
    pub identity_token: String,
}

impl Auth {
    /// Decode X-Registry-Auth (base64url JSON). Empty or anonymous -> None.
    pub fn from_header(h: &str) -> Option<Self> {
        let raw = crate::util::base64_decode(h)?;
        let v: Value = serde_json::from_slice(&raw).ok()?;
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let a = Auth { username: s("username"), password: s("password"), identity_token: s("identitytoken") };
        (!a.username.is_empty() || !a.identity_token.is_empty()).then_some(a)
    }
}

struct Session<'a> {
    client: &'a Client,
    r: &'a Reference,
    base: String,
    auth: Option<Auth>,
    token: Mutex<Option<String>>,
}

impl Session<'_> {
    fn authorization(&self) -> Option<String> {
        self.token.lock().unwrap().clone().map(|t| format!("Bearer {t}"))
    }

    fn fetch_token(&self, challenge: &str) -> Result<()> {
        let c = parse_challenge(challenge).ok_or_else(|| Error::msg(format!("bad auth challenge: {challenge}")))?;
        if c.scheme == "basic" {
            let a = self.auth.as_ref().ok_or_else(|| Error::msg("registry requires credentials (docker login)"))?;
            *self.token.lock().unwrap() = None;
            return Err(Error::msg(format!(
                "registry wants basic auth; not supported for {} (user {})",
                self.r.domain, a.username
            )));
        }
        let mut headers = Vec::new();
        if let Some(a) = &self.auth {
            if !a.username.is_empty() {
                headers.push(("Authorization", format!("Basic {}", base64_encode(format!("{}:{}", a.username, a.password).as_bytes()))));
            }
        }
        let resp = self.client.get(&token_url(&c, &self.r.repo), &headers)?;
        let status = resp.status();
        let body = resp.bytes(1 << 20)?;
        if status != 200 {
            return Err(Error::msg(format!("token request to {} failed: HTTP {status} {}", c.realm, String::from_utf8_lossy(&body).trim())));
        }
        let v: Value = serde_json::from_slice(&body).map_err(|e| Error::msg(format!("token response: {e}")))?;
        let token = v.get("token").or_else(|| v.get("access_token")).and_then(Value::as_str);
        *self.token.lock().unwrap() = Some(token.ok_or_else(|| Error::msg("token response without token"))?.to_string());
        Ok(())
    }

    /// GET with auth; on 401 obtain a token once and retry.
    fn get(&self, path: &str, accept: Option<&str>) -> Result<Response> {
        let url = format!("{}{path}", self.base);
        for attempt in 0..2 {
            let mut headers = Vec::new();
            if let Some(a) = accept {
                headers.push(("Accept", a.to_string()));
            }
            if let Some(a) = self.authorization() {
                headers.push(("Authorization", a));
            }
            let resp = self.client.get(&url, &headers)?;
            if resp.status() == 401 && attempt == 0 {
                let challenge = resp.header("www-authenticate").unwrap_or_default().to_string();
                self.fetch_token(&challenge)?;
                continue;
            }
            return match resp.status() {
                200 => Ok(resp),
                404 => Err(Error::msg(format!("manifest unknown: {} not found", self.r))),
                401 | 403 => Err(Error::msg(format!(
                    "pull access denied for {}, repository does not exist or may require 'docker login'",
                    self.r.familiar_repo()
                ))),
                s => {
                    let body = resp.bytes(64 << 10).unwrap_or_default();
                    Err(Error::msg(format!("GET {url}: HTTP {s} {}", String::from_utf8_lossy(&body).trim())))
                }
            };
        }
        unreachable!()
    }

    fn manifest(&self, reference: &str) -> Result<(Vec<u8>, String)> {
        let resp = self.get(&format!("/v2/{}/manifests/{reference}", self.r.repo), Some(&manifest_accept()))?;
        let ct = resp.header("content-type").unwrap_or_default().to_string();
        Ok((resp.bytes(MAX_MANIFEST)?, ct))
    }

    fn blob(&self, digest: &str) -> Result<Response> {
        self.get(&format!("/v2/{}/blobs/{digest}", self.r.repo), None)
    }
}

/// sha256 of everything read through it.
struct Hashing<R> {
    inner: R,
    hasher: Sha256,
    bytes: u64,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}

fn check_digest(expected: &str, got_hex: &str, what: &str) -> Result<()> {
    if expected.strip_prefix("sha256:") != Some(got_hex) {
        return Err(Error::msg(format!("{what}: digest mismatch (expected {expected}, got sha256:{got_hex})")));
    }
    Ok(())
}

fn short(digest: &str) -> String {
    digest.strip_prefix("sha256:").unwrap_or(digest).chars().take(12).collect()
}

/// Download one layer and extract it into the store.
fn pull_layer(s: &Session, store: &Store, desc: &Descriptor, diff_id: &str) -> Result<u64> {
    let diff_hex = diff_id.strip_prefix("sha256:").ok_or_else(|| Error::msg(format!("bad diff id {diff_id}")))?;
    let compression = layer_compression(&desc.media_type).map_err(Error::msg)?;
    let resp = s.blob(&desc.digest)?;
    let mut compressed = Hashing { inner: BufReader::with_capacity(256 << 10, resp.body), hasher: Sha256::new(), bytes: 0 };
    let tmp = store.tmp_dir()?;
    let result = (|| -> Result<()> {
        let mut plain: Box<dyn Read> = match compression {
            Compression::Gzip => Box::new(GzDecoder::new(&mut compressed)),
            Compression::None => Box::new(&mut compressed),
        };
        let mut uncompressed = Hashing { inner: &mut plain, hasher: Sha256::new(), bytes: 0 };
        extract_layer(&mut uncompressed, &tmp)?;
        // Drain tar padding so both hashes cover the whole stream.
        std::io::copy(&mut uncompressed, &mut std::io::sink())?;
        let diff = hex(&uncompressed.hasher.clone().finalize());
        drop(uncompressed);
        drop(plain);
        std::io::copy(&mut compressed, &mut std::io::sink())?;
        check_digest(&desc.digest, &hex(&compressed.hasher.clone().finalize()), "layer")?;
        check_digest(diff_id, &diff, "layer contents")
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    store.commit_layer(&tmp, diff_hex)?;
    Ok(du(&store.layer_path(diff_hex)))
}

/// Bytes used by a directory tree (regular files only).
pub fn du(p: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map_or(0, |m| m.len()),
                _ => {}
            }
        }
    }
    total
}

/// Pull `r` into `store`. `progress` receives Docker JSON progress messages.
/// Returns the image id.
pub fn pull(
    client: &Client,
    store: &Store,
    r: &Reference,
    auth: Option<Auth>,
    parallel: usize,
    progress: &mut (dyn FnMut(Value) + Send),
) -> Result<String> {
    let scheme = if r.insecure() { "http" } else { "https" };
    let s = Session { client, r, base: format!("{scheme}://{}", r.api_host()), auth, token: Mutex::new(None) };
    progress(json!({"status": format!("Pulling from {}", r.familiar_repo()), "id": r.tag.as_deref().unwrap_or(r.manifest_ref())}));

    let (top, ct) = s.manifest(r.manifest_ref())?;
    let top_digest = format!("sha256:{}", hex(&Sha256::digest(&top)));
    if let Some(d) = &r.digest {
        check_digest(d, top_digest.strip_prefix("sha256:").unwrap(), "manifest")?;
    }
    let parsed: AnyManifest = serde_json::from_slice(&top).map_err(|e| Error::msg(format!("manifest: {e}")))?;
    let manifest = if parsed.is_index(&ct) {
        let d = select_platform(&parsed.manifests).ok_or_else(|| {
            Error::msg(format!(
                "no matching manifest for linux/arm/v7 in the manifest list entries (available: {})",
                platforms(&parsed.manifests)
            ))
        })?;
        let (bytes, _) = s.manifest(&d.digest)?;
        check_digest(&d.digest, &hex(&Sha256::digest(&bytes)), "platform manifest")?;
        serde_json::from_slice::<AnyManifest>(&bytes).map_err(|e| Error::msg(format!("manifest: {e}")))?
    } else {
        parsed
    };
    let config_desc = manifest.config.clone().ok_or_else(|| Error::msg("manifest has no config (schema 1 images are not supported)"))?;
    let config_blob = s.blob(&config_desc.digest)?.bytes(MAX_MANIFEST)?;
    check_digest(&config_desc.digest, &hex(&Sha256::digest(&config_blob)), "image config")?;
    let config: ImageConfigFile = serde_json::from_slice(&config_blob).map_err(|e| Error::msg(format!("image config: {e}")))?;
    if !config.architecture.is_empty() && config.architecture != "arm" {
        return Err(Error::msg(format!(
            "image is for {}/{}, this board runs linux/arm/v7",
            config.os, config.architecture
        )));
    }
    let diff_ids = config.rootfs.clone().unwrap_or_default().diff_ids;
    if diff_ids.len() != manifest.layers.len() {
        return Err(Error::msg(format!("image has {} layers but {} diff ids", manifest.layers.len(), diff_ids.len())));
    }
    let id = config_desc.digest.clone();
    let already = store.load(&id).is_some();

    let todo: Vec<usize> = (0..diff_ids.len())
        .filter(|&i| {
            let have = store.has_layer(diff_ids[i].strip_prefix("sha256:").unwrap_or(&diff_ids[i]));
            if have {
                progress(json!({"status": "Already exists", "id": short(&manifest.layers[i].digest)}));
            }
            !have
        })
        .collect();
    for &i in &todo {
        progress(json!({"status": "Pulling fs layer", "id": short(&manifest.layers[i].digest)}));
    }
    let next = AtomicUsize::new(0);
    let first_error: Mutex<Option<Error>> = Mutex::new(None);
    let progress = Mutex::new(progress);
    std::thread::scope(|scope| {
        for _ in 0..parallel.clamp(1, 4).min(todo.len().max(1)) {
            scope.spawn(|| loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                if k >= todo.len() || first_error.lock().unwrap().is_some() {
                    break;
                }
                let i = todo[k];
                let desc = &manifest.layers[i];
                (progress.lock().unwrap())(json!({"status": "Downloading", "id": short(&desc.digest), "progressDetail": {"current": 0, "total": desc.size}}));
                match pull_layer(&s, store, desc, &diff_ids[i]) {
                    Ok(_) => (progress.lock().unwrap())(json!({"status": "Pull complete", "id": short(&desc.digest)})),
                    Err(e) => {
                        first_error.lock().unwrap().get_or_insert(e);
                    }
                }
            });
        }
    });
    let progress = progress.into_inner().unwrap();
    if let Some(e) = first_error.into_inner().unwrap() {
        return Err(e);
    }

    let layers: Vec<String> = diff_ids.iter().map(|d| d.strip_prefix("sha256:").unwrap_or(d).to_string()).collect();
    let size = layers.iter().map(|l| du(&store.layer_path(l))).sum();
    let meta = ImageMeta {
        layers,
        size,
        created: Store::created_ns(&config),
        repo_digests: vec![format!("{}@{top_digest}", r.full_repo())],
    };
    store.save_image(&id, &config_blob, &meta)?;
    let replaced = match r.tag_key() {
        Some(k) => store.tag(&k, &id)?,
        None => None,
    };
    progress(json!({"status": format!("Digest: {top_digest}")}));
    let status = if already && replaced.is_none() {
        format!("Status: Image is up to date for {r}")
    } else {
        format!("Status: Downloaded newer image for {r}")
    };
    progress(json!({"status": status}));
    info!("engine", "pulled {r} ({id})");
    Ok(id)
}
