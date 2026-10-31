//! `pios ctld`: serves the control protocol over mutual TLS so the board can
//! be managed remotely even when the container engine is down. Each request
//! line is relayed verbatim to PID 1's unix socket; PID 1 stays TLS-free.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use pios_core::config::PiosConfig;
use pios_core::{info, paths, warn, Error, Result};

const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounded concurrency: this is a rescue channel, not a public service.
const MAX_CLIENTS: usize = 4;

pub fn run(cfg: &PiosConfig) -> Result<()> {
    if !cfg.remote.enabled {
        return Ok(());
    }
    let dir = Path::new(paths::TLS_DIR);
    if !["ca.pem", "server-cert.pem", "server-key.pem"].iter().all(|f| dir.join(f).exists()) {
        info!("ctld", "no certificates in {}, remote management disabled", paths::TLS_DIR);
        return Ok(());
    }
    let config = Arc::new(tls_config(dir)?);
    let listener = TcpListener::bind(("0.0.0.0", cfg.remote.port))
        .map_err(|e| Error::msg(format!("bind :{}: {e}", cfg.remote.port)))?;
    info!("ctld", "remote management on :{} (mTLS)", cfg.remote.port);
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for stream in listener.incoming().flatten() {
        use std::sync::atomic::Ordering;
        if active.load(Ordering::Relaxed) >= MAX_CLIENTS {
            continue; // dropping the stream closes it
        }
        active.fetch_add(1, Ordering::Relaxed);
        let (config, active) = (config.clone(), active.clone());
        std::thread::spawn(move || {
            let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
            if let Err(e) = serve(stream, config) {
                warn!("ctld", "{peer}: {e}");
            }
            active.fetch_sub(1, Ordering::Relaxed);
        });
    }
    Ok(())
}

/// Mutual TLS server configuration from `ca.pem`, `server-cert.pem` and
/// `server-key.pem` in `dir` (also used by the native engine on :2376).
pub fn tls_config(dir: &Path) -> Result<ServerConfig> {
    let pem_err = |what: &str, e: rustls_pki_types::pem::Error| Error::msg(format!("{what}: {e}"));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_file_iter(dir.join("ca.pem")).map_err(|e| pem_err("ca.pem", e))? {
        roots.add(ca.map_err(|e| pem_err("ca.pem", e))?).map_err(|e| Error::msg(format!("ca.pem: {e}")))?;
    }
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| Error::msg(format!("client verifier: {e}")))?;
    let certs = CertificateDer::pem_file_iter(dir.join("server-cert.pem"))
        .map_err(|e| pem_err("server-cert.pem", e))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| pem_err("server-cert.pem", e))?;
    let key = PrivateKeyDer::from_pem_file(dir.join("server-key.pem")).map_err(|e| pem_err("server-key.pem", e))?;
    ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::msg(e.to_string()))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| Error::msg(format!("server certificate: {e}")))
}

fn serve(tcp: TcpStream, config: Arc<ServerConfig>) -> Result<()> {
    tcp.set_read_timeout(Some(IO_TIMEOUT))?;
    tcp.set_write_timeout(Some(IO_TIMEOUT))?;
    let conn = ServerConnection::new(config).map_err(|e| Error::msg(e.to_string()))?;
    let mut tls = BufReader::new(StreamOwned::new(conn, tcp));
    let mut request = String::new();
    tls.read_line(&mut request)?;
    let response = relay(&request)?;
    let tls = tls.get_mut();
    tls.write_all(response.as_bytes())?;
    tls.conn.send_close_notify();
    tls.flush()?;
    Ok(())
}

/// Forward one request line to PID 1 and return its response line.
fn relay(request: &str) -> Result<String> {
    let mut local = UnixStream::connect(paths::CTL_SOCKET).map_err(|e| Error::msg(format!("ctl socket: {e}")))?;
    local.set_read_timeout(Some(IO_TIMEOUT))?;
    local.write_all(request.trim_end().as_bytes())?;
    local.write_all(b"\n")?;
    let mut response = String::new();
    BufReader::new(local).read_line(&mut response)?;
    Ok(response)
}
