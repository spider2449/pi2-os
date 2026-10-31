//! Blocking HTTPS GET client for registries: one connection per request,
//! redirects followed (blob downloads usually go to a CDN).

use std::io::{BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName};

use pios_core::{Error, Result};

use crate::http::{body_reader, read_response_head, ResponseHead};

const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REDIRECTS: usize = 5;

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

pub struct Response {
    pub head: ResponseHead,
    pub body: Box<dyn Read + Send>,
    /// URL that produced this response (after redirects).
    pub url: String,
}

impl Response {
    pub fn status(&self) -> u16 {
        self.head.status
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.head.header(name)
    }

    /// Whole body, bounded (manifests, configs, tokens).
    pub fn bytes(self, limit: u64) -> Result<Vec<u8>> {
        let mut v = Vec::new();
        self.body.take(limit + 1).read_to_end(&mut v)?;
        if v.len() as u64 > limit {
            return Err(Error::msg(format!("response from {} larger than {limit} bytes", self.url)));
        }
        Ok(v)
    }
}

#[derive(Clone)]
pub struct Client {
    tls: Arc<ClientConfig>,
}

struct Url<'a> {
    https: bool,
    host: &'a str,
    port: u16,
    path: &'a str,
}

fn parse_url(url: &str) -> Result<Url<'_>> {
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(Error::msg(format!("unsupported URL {url}")));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => {
            (h, p.parse().map_err(|_| Error::msg(format!("bad port in {url}")))?)
        }
        _ => (authority, if https { 443 } else { 80 }),
    };
    Ok(Url { https, host, port, path })
}

/// Resolve a Location header against the request URL.
fn join(base: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_string();
    }
    let origin_end = base.find("://").map_or(0, |i| i + 3);
    let host_end = base[origin_end..].find('/').map_or(base.len(), |i| origin_end + i);
    if location.starts_with('/') {
        format!("{}{location}", &base[..host_end])
    } else {
        let dir_end = base.rfind('/').filter(|i| *i >= host_end).map_or(host_end, |i| i);
        format!("{}/{location}", &base[..dir_end])
    }
}

impl Client {
    pub fn new() -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = RootCertStore::empty();
        let certs = CertificateDer::pem_file_iter(CA_BUNDLE)
            .map_err(|e| Error::msg(format!("{CA_BUNDLE}: {e}")))?
            .filter_map(|c| c.ok());
        let (added, _) = roots.add_parsable_certificates(certs);
        if added == 0 {
            return Err(Error::msg(format!("no usable CA certificates in {CA_BUNDLE}")));
        }
        let tls = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::msg(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self { tls: Arc::new(tls) })
    }

    fn connect(&self, u: &Url) -> Result<Box<dyn Stream>> {
        let addrs: Vec<_> = (u.host, u.port)
            .to_socket_addrs()
            .map_err(|e| Error::msg(format!("resolve {}: {e}", u.host)))?
            .collect();
        let mut last = None;
        let mut tcp = None;
        for a in addrs.iter().filter(|a| a.is_ipv4()).chain(addrs.iter().filter(|a| a.is_ipv6())) {
            match TcpStream::connect_timeout(a, CONNECT_TIMEOUT) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let tcp = tcp.ok_or_else(|| {
            Error::msg(format!("connect {}:{}: {}", u.host, u.port, last.map_or("no address".into(), |e| e.to_string())))
        })?;
        tcp.set_read_timeout(Some(IO_TIMEOUT))?;
        tcp.set_write_timeout(Some(IO_TIMEOUT))?;
        let _ = tcp.set_nodelay(true);
        if !u.https {
            return Ok(Box::new(tcp));
        }
        let name = ServerName::try_from(u.host.to_string()).map_err(|e| Error::msg(format!("{}: {e}", u.host)))?;
        let conn = ClientConnection::new(self.tls.clone(), name).map_err(|e| Error::msg(e.to_string()))?;
        Ok(Box::new(StreamOwned::new(conn, tcp)))
    }

    /// GET `url` with extra headers. `Authorization` is only sent to the
    /// original host, never to a redirect target (CDN URLs are pre-signed).
    pub fn get(&self, url: &str, headers: &[(&str, String)]) -> Result<Response> {
        let mut url = url.to_string();
        let origin = parse_url(&url)?.host.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let u = parse_url(&url)?;
            let same_host = u.host == origin;
            let mut stream = self.connect(&u)?;
            let host_header = if (u.https && u.port == 443) || (!u.https && u.port == 80) {
                u.host.to_string()
            } else {
                format!("{}:{}", u.host, u.port)
            };
            let mut req = format!(
                "GET {} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: pios-native/{}\r\nConnection: close\r\n",
                u.path,
                env!("CARGO_PKG_VERSION")
            );
            for (k, v) in headers {
                if k.eq_ignore_ascii_case("authorization") && !same_host {
                    continue;
                }
                req.push_str(&format!("{k}: {v}\r\n"));
            }
            req.push_str("\r\n");
            stream.write_all(req.as_bytes()).map_err(|e| Error::msg(format!("GET {url}: {e}")))?;
            stream.flush()?;
            let mut reader = BufReader::with_capacity(64 << 10, stream);
            let head = read_response_head(&mut reader).map_err(|e| Error::msg(format!("GET {url}: {e}")))?;
            if matches!(head.status, 301 | 302 | 303 | 307 | 308) {
                let loc = head.header("location").ok_or_else(|| Error::msg(format!("redirect without Location from {url}")))?;
                url = join(&url, loc);
                continue;
            }
            let body = body_reader(&head, reader);
            return Ok(Response { head, body, url });
        }
        Err(Error::msg(format!("too many redirects for {url}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_redirects() {
        let u = parse_url("https://registry-1.docker.io/v2/library/alpine/manifests/latest").unwrap();
        assert!(u.https);
        assert_eq!((u.host, u.port, u.path), ("registry-1.docker.io", 443, "/v2/library/alpine/manifests/latest"));
        let u = parse_url("http://localhost:5000").unwrap();
        assert_eq!((u.host, u.port, u.path), ("localhost", 5000, "/"));
        assert_eq!(join("https://a.io/v2/x/blobs/sha", "https://cdn.io/y?sig=1"), "https://cdn.io/y?sig=1");
        assert_eq!(join("https://a.io/v2/x/blobs/sha", "/other"), "https://a.io/other");
        assert_eq!(join("https://a.io/v2/x/blobs/sha", "rel"), "https://a.io/v2/x/blobs/rel");
    }
}
