//! Image references: `alpine`, `nginx:1.27`, `ghcr.io/o/app@sha256:...`.

use std::fmt;

pub const DOCKER_HUB: &str = "docker.io";
/// Docker Hub's registry API endpoint.
const DOCKER_HUB_API: &str = "registry-1.docker.io";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Registry host[:port], `docker.io` for Docker Hub.
    pub domain: String,
    /// Repository path, `library/alpine` for official images.
    pub repo: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl Reference {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() || s.chars().any(char::is_whitespace) {
            return Err(format!("invalid reference format: {s:?}"));
        }
        let (name, digest) = match s.split_once('@') {
            Some((n, d)) => {
                let hex = d.strip_prefix("sha256:").ok_or_else(|| format!("unsupported digest: {d}"))?;
                if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("invalid digest: {d}"));
                }
                (n, Some(d.to_ascii_lowercase()))
            }
            None => (s, None),
        };
        // A tag follows the last ':' after the last '/' (a ':' before it is a port).
        let last_slash = name.rfind('/').map_or(0, |i| i + 1);
        let (name, tag) = match name[last_slash..].rfind(':') {
            Some(i) => (&name[..last_slash + i], Some(name[last_slash + i + 1..].to_string())),
            None => (name, None),
        };
        if name.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(format!("invalid reference format: repository name must be lowercase: {s}"));
        }
        let (domain, repo) = match name.split_once('/') {
            Some((first, rest)) if first.contains('.') || first.contains(':') || first == "localhost" => {
                (first.to_string(), rest.to_string())
            }
            _ => (DOCKER_HUB.to_string(), name.to_string()),
        };
        let domain = if domain == "index.docker.io" { DOCKER_HUB.to_string() } else { domain };
        let repo = if domain == DOCKER_HUB && !repo.contains('/') { format!("library/{repo}") } else { repo };
        let valid = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-/".contains(c);
        if repo.is_empty() || !repo.chars().all(valid) || repo.starts_with('/') || repo.ends_with('/') {
            return Err(format!("invalid reference format: {s}"));
        }
        if let Some(t) = &tag {
            if t.is_empty() || t.len() > 128 || !t.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
                return Err(format!("invalid tag: {t:?}"));
            }
        }
        let tag = if tag.is_none() && digest.is_none() { Some("latest".to_string()) } else { tag };
        Ok(Self { domain, repo, tag, digest })
    }

    /// Host to talk to for the registry API.
    pub fn api_host(&self) -> &str {
        if self.domain == DOCKER_HUB {
            DOCKER_HUB_API
        } else {
            &self.domain
        }
    }

    /// Plain HTTP only for local registries, as dockerd does by default.
    pub fn insecure(&self) -> bool {
        let host = self.domain.split(':').next().unwrap_or_default();
        host == "localhost" || host == "127.0.0.1"
    }

    /// Fully qualified repository, `docker.io/library/alpine`.
    pub fn full_repo(&self) -> String {
        format!("{}/{}", self.domain, self.repo)
    }

    /// Repository as Docker displays it: `alpine`, `user/app`, `ghcr.io/o/app`.
    pub fn familiar_repo(&self) -> String {
        if self.domain == DOCKER_HUB {
            self.repo.strip_prefix("library/").unwrap_or(&self.repo).to_string()
        } else {
            self.full_repo()
        }
    }

    /// The tag or digest to fetch the manifest by.
    pub fn manifest_ref(&self) -> &str {
        self.digest.as_deref().or(self.tag.as_deref()).unwrap_or("latest")
    }

    /// Key in the tag store, `docker.io/library/alpine:latest`.
    pub fn tag_key(&self) -> Option<String> {
        self.tag.as_ref().map(|t| format!("{}:{t}", self.full_repo()))
    }
}

impl fmt::Display for Reference {
    /// Familiar form: `alpine:latest`, `ghcr.io/o/app@sha256:...`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.familiar_repo())?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

/// `docker.io/library/alpine:latest` -> `alpine:latest` for display.
pub fn familiar(full: &str) -> String {
    match Reference::parse(full) {
        Ok(r) => r.to_string(),
        Err(_) => full.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> Reference {
        Reference::parse(s).unwrap()
    }

    #[test]
    fn docker_hub_shorthands() {
        let a = r("alpine");
        assert_eq!((a.domain.as_str(), a.repo.as_str(), a.tag.as_deref()), ("docker.io", "library/alpine", Some("latest")));
        assert_eq!(a.api_host(), "registry-1.docker.io");
        assert_eq!(a.to_string(), "alpine:latest");
        assert_eq!(r("user/app:1.0").repo, "user/app");
        assert_eq!(r("docker.io/library/nginx:1.27").to_string(), "nginx:1.27");
        assert_eq!(r("index.docker.io/library/nginx").tag_key().unwrap(), "docker.io/library/nginx:latest");
    }

    #[test]
    fn other_registries_ports_and_digests() {
        let g = r("ghcr.io/owner/app:v2");
        assert_eq!((g.domain.as_str(), g.repo.as_str()), ("ghcr.io", "owner/app"));
        assert_eq!(g.to_string(), "ghcr.io/owner/app:v2");
        let l = r("localhost:5000/test");
        assert_eq!((l.domain.as_str(), l.tag.as_deref()), ("localhost:5000", Some("latest")));
        assert!(l.insecure() && !g.insecure());
        let d = format!("alpine@sha256:{}", "a".repeat(64));
        let d = r(&d);
        assert_eq!(d.tag, None);
        assert!(d.manifest_ref().starts_with("sha256:"));
    }

    #[test]
    fn rejects_bad_references() {
        assert!(Reference::parse("").is_err());
        assert!(Reference::parse("Alpine").is_err());
        assert!(Reference::parse("alpine:").is_err());
        assert!(Reference::parse("alpine@sha256:abc").is_err());
        assert!(Reference::parse("a b").is_err());
    }
}
