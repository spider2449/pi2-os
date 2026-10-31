//! Registry protocol pieces that need no network: manifest formats, the
//! platform choice for the Pi 2 and token authentication challenges.

use serde::Deserialize;

pub const MEDIA_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MEDIA_DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const MEDIA_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MEDIA_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// Accept header for manifest requests.
pub fn manifest_accept() -> String {
    [MEDIA_OCI_INDEX, MEDIA_DOCKER_LIST, MEDIA_OCI_MANIFEST, MEDIA_DOCKER_MANIFEST].join(", ")
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Platform {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub digest: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub platform: Option<Platform>,
}

/// Image index / manifest list, or a single image manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct AnyManifest {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    #[serde(default)]
    pub manifests: Vec<Descriptor>,
    #[serde(default)]
    pub config: Option<Descriptor>,
    #[serde(default)]
    pub layers: Vec<Descriptor>,
}

impl AnyManifest {
    /// Index if it lists manifests (some registries omit mediaType).
    pub fn is_index(&self, content_type: &str) -> bool {
        let t = if self.media_type.is_empty() { content_type } else { &self.media_type };
        t.starts_with(MEDIA_OCI_INDEX) || t.starts_with(MEDIA_DOCKER_LIST) || (self.config.is_none() && !self.manifests.is_empty())
    }
}

/// Pick the best manifest for linux/arm/v7. ARMv6 and v5 binaries run on the
/// Cortex-A7 too, so they are acceptable fallbacks.
pub fn select_platform(manifests: &[Descriptor]) -> Option<&Descriptor> {
    let rank = |d: &Descriptor| -> Option<u8> {
        let p = d.platform.as_ref()?;
        if p.os != "linux" || p.architecture != "arm" {
            return None;
        }
        match p.variant.as_deref() {
            Some("v7") => Some(0),
            None | Some("") => Some(1),
            Some("v6") => Some(2),
            Some("v5") => Some(3),
            _ => None,
        }
    };
    manifests.iter().filter_map(|d| rank(d).map(|r| (r, d))).min_by_key(|(r, _)| *r).map(|(_, d)| d)
}

/// Platforms an index offers, for the "no matching manifest" error.
pub fn platforms(manifests: &[Descriptor]) -> String {
    let v: Vec<String> = manifests
        .iter()
        .filter_map(|d| d.platform.as_ref())
        .filter(|p| p.os != "unknown")
        .map(|p| match &p.variant {
            Some(v) => format!("{}/{}/{v}", p.os, p.architecture),
            None => format!("{}/{}", p.os, p.architecture),
        })
        .collect();
    v.join(", ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Gzip,
    None,
}

pub fn layer_compression(media_type: &str) -> Result<Compression, String> {
    if media_type.ends_with("gzip") || media_type.ends_with(".tar.gzip") || media_type.is_empty() {
        Ok(Compression::Gzip)
    } else if media_type.ends_with(".tar") || media_type.ends_with("tar") {
        Ok(Compression::None)
    } else {
        Err(format!("unsupported layer type {media_type}"))
    }
}

/// Image configuration blob (the subset PiOS uses).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ImageConfigFile {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub config: Option<ImageRunConfig>,
    #[serde(default)]
    pub rootfs: Option<RootFs>,
}

#[derive(Debug, Clone, Deserialize, Default, serde::Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ImageRunConfig {
    pub user: Option<String>,
    pub env: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub exposed_ports: Option<serde_json::Map<String, serde_json::Value>>,
    pub labels: Option<std::collections::BTreeMap<String, String>>,
    pub stop_signal: Option<String>,
    pub volumes: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RootFs {
    #[serde(default)]
    pub diff_ids: Vec<String>,
}

/// A `WWW-Authenticate: Bearer realm=...,service=...,scope=...` challenge.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Challenge {
    pub scheme: String,
    pub realm: String,
    pub service: Option<String>,
    pub scope: Option<String>,
}

pub fn parse_challenge(header: &str) -> Option<Challenge> {
    let (scheme, rest) = header.trim().split_once(' ').unwrap_or((header.trim(), ""));
    let mut c = Challenge { scheme: scheme.to_ascii_lowercase(), ..Default::default() };
    let mut rest = rest.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let key = key.trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let (value, tail) = if let Some(q) = after.strip_prefix('"') {
            let end = q.find('"')?;
            (&q[..end], &q[end + 1..])
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (&after[..end], &after[end..])
        };
        match key.as_str() {
            "realm" => c.realm = value.to_string(),
            "service" => c.service = Some(value.to_string()),
            "scope" => c.scope = Some(value.to_string()),
            _ => {}
        }
        rest = tail.trim_start_matches(',').trim();
    }
    Some(c)
}

/// Token endpoint URL for a challenge and repository.
pub fn token_url(c: &Challenge, repo: &str) -> String {
    let scope = c.scope.clone().unwrap_or_else(|| format!("repository:{repo}:pull"));
    let mut url = format!("{}?scope={}", c.realm, crate::http::percent_encode(&scope));
    if let Some(s) = &c.service {
        url.push_str(&format!("&service={}", crate::http::percent_encode(s)));
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(arch: &str, variant: Option<&str>) -> Descriptor {
        Descriptor {
            media_type: MEDIA_OCI_MANIFEST.into(),
            digest: format!("sha256:{arch}{}", variant.unwrap_or("")),
            size: 1,
            platform: Some(Platform { architecture: arch.into(), os: "linux".into(), variant: variant.map(Into::into) }),
        }
    }

    #[test]
    fn prefers_armv7_then_older_arm() {
        let list = vec![d("amd64", None), d("arm", Some("v6")), d("arm", Some("v7")), d("arm64", Some("v8"))];
        assert_eq!(select_platform(&list).unwrap().digest, "sha256:armv7");
        let list = vec![d("amd64", None), d("arm", Some("v6"))];
        assert_eq!(select_platform(&list).unwrap().digest, "sha256:armv6");
        assert!(select_platform(&[d("amd64", None), d("arm64", Some("v8"))]).is_none());
        assert_eq!(platforms(&[d("amd64", None), d("arm64", Some("v8"))]), "linux/amd64, linux/arm64/v8");
    }

    #[test]
    fn parses_docker_hub_challenge() {
        let c = parse_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#,
        )
        .unwrap();
        assert_eq!(c.scheme, "bearer");
        assert_eq!(c.realm, "https://auth.docker.io/token");
        assert_eq!(c.service.as_deref(), Some("registry.docker.io"));
        assert_eq!(
            token_url(&c, "library/alpine"),
            "https://auth.docker.io/token?scope=repository%3Alibrary%2Falpine%3Apull&service=registry.docker.io"
        );
        let ghcr = parse_challenge(r#"Bearer realm="https://ghcr.io/token",service="ghcr.io""#).unwrap();
        assert!(token_url(&ghcr, "o/app").contains("scope=repository%3Ao%2Fapp%3Apull"));
        assert_eq!(parse_challenge("Basic realm=\"x\"").unwrap().scheme, "basic");
    }

    #[test]
    fn manifest_kinds_and_layers() {
        let idx: AnyManifest = serde_json::from_str(r#"{"schemaVersion":2,"manifests":[{"digest":"sha256:a","size":1}]}"#).unwrap();
        assert!(idx.is_index(""));
        let m: AnyManifest = serde_json::from_str(
            r#"{"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{"digest":"sha256:c","size":2},"layers":[]}"#,
        )
        .unwrap();
        assert!(!m.is_index(MEDIA_DOCKER_MANIFEST));
        assert_eq!(layer_compression("application/vnd.docker.image.rootfs.diff.tar.gzip"), Ok(Compression::Gzip));
        assert_eq!(layer_compression("application/vnd.oci.image.layer.v1.tar"), Ok(Compression::None));
        assert!(layer_compression("application/vnd.oci.image.layer.v1.tar+zstd").is_err());
    }
}
