//! Readiness probes.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use pios_core::service::ReadyCheck;

/// Probe a running service. `Exited` is resolved by the exit handler instead.
pub fn probe(check: &ReadyCheck) -> bool {
    match check {
        ReadyCheck::Started => true,
        ReadyCheck::PathExists(p) => Path::new(p).exists(),
        ReadyCheck::HttpUnix { socket, path } => http_unix_ok(socket, path),
        ReadyCheck::Exited => false,
    }
}

/// `GET path` over a unix socket; true on an HTTP 200 status line.
pub fn http_unix_ok(socket: &str, path: &str) -> bool {
    let Ok(mut s) = UnixStream::connect(socket) else { return false };
    let timeout = Some(Duration::from_millis(500));
    let _ = s.set_read_timeout(timeout);
    let _ = s.set_write_timeout(timeout);
    let req = format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 16];
    let mut n = 0;
    while n < head.len() {
        match s.read(&mut head[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    let line = &head[..n];
    line.starts_with(b"HTTP/1.") && line.get(9..12) == Some(b"200")
}
