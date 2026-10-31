//! Minimal Docker Engine API client over the unix socket, used by the
//! shutdown hook (`pios engine-stop`) to stop containers gracefully before
//! dockerd exits (live-restore would otherwise leave them running).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use pios_core::{info, warn, Error, Result};

use crate::DOCKER_SOCKET;

/// Seconds each container gets between SIGTERM and SIGKILL.
const STOP_GRACE_SECS: u32 = 10;

fn request(method: &str, path: &str) -> Result<(u16, String)> {
    let mut s = UnixStream::connect(DOCKER_SOCKET).map_err(|e| Error::msg(format!("connect docker: {e}")))?;
    s.set_read_timeout(Some(Duration::from_secs(STOP_GRACE_SECS as u64 + 5)))?;
    // HTTP/1.0 keeps the response un-chunked and closes the connection.
    write!(s, "{method} {path} HTTP/1.0\r\nHost: docker\r\nContent-Length: 0\r\n\r\n")?;
    let mut raw = String::new();
    s.read_to_string(&mut raw)?;
    let status = raw.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    Ok((status, body))
}

/// Stop all running containers in parallel and wait for them.
pub fn stop_all_containers() -> Result<()> {
    let (status, body) = request("GET", "/containers/json")?;
    if status != 200 {
        return Err(Error::msg(format!("list containers: HTTP {status}")));
    }
    let list: serde_json::Value = serde_json::from_str(&body).map_err(|e| Error::msg(e.to_string()))?;
    let ids: Vec<String> = list
        .as_array()
        .map(|a| a.iter().filter_map(|c| c["Id"].as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    info!("engine", "stopping {} container(s)", ids.len());
    let handles: Vec<_> = ids
        .into_iter()
        .map(|id| {
            std::thread::spawn(move || {
                let path = format!("/containers/{id}/stop?t={STOP_GRACE_SECS}");
                match request("POST", &path) {
                    Ok((204 | 304, _)) => {}
                    Ok((code, body)) => warn!("engine", "stop {}: HTTP {code} {}", &id[..12.min(id.len())], body.trim()),
                    Err(e) => warn!("engine", "stop {}: {e}", &id[..12.min(id.len())]),
                }
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}
