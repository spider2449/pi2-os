//! Embedded DNS for user-defined networks: containers resolve each other by
//! name, alias or hostname; everything else goes to the host's resolvers.
//! The wire format helpers are pure; `serve` runs one UDP socket per network
//! on its gateway address.

use std::net::Ipv4Addr;

const TYPE_A: u16 = 1;
#[cfg(test)]
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

#[derive(Debug, PartialEq)]
pub struct Question {
    pub id: u16,
    /// Lowercase, without the trailing dot.
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
    /// Byte offset just past the question section.
    pub end: usize,
}

/// First question of a standard query, or None.
pub fn parse_query(msg: &[u8]) -> Option<Question> {
    if msg.len() < 12 || msg[2] & 0x80 != 0 || u16::from_be_bytes([msg[4], msg[5]]) != 1 {
        return None;
    }
    let mut i = 12;
    let mut labels = Vec::new();
    loop {
        let len = *msg.get(i)? as usize;
        i += 1;
        if len == 0 {
            break;
        }
        if len & 0xc0 != 0 {
            return None; // no compression inside a question
        }
        labels.push(String::from_utf8_lossy(msg.get(i..i + len)?).to_ascii_lowercase());
        i += len;
    }
    let qtype = u16::from_be_bytes([*msg.get(i)?, *msg.get(i + 1)?]);
    let qclass = u16::from_be_bytes([*msg.get(i + 2)?, *msg.get(i + 3)?]);
    Some(Question { id: u16::from_be_bytes([msg[0], msg[1]]), name: labels.join("."), qtype, qclass, end: i + 4 })
}

/// Authoritative answer for a known container name: A records for `ips`
/// (none for other types, so AAAA lookups return quickly with NOERROR).
pub fn answer(query: &[u8], q: &Question, ips: &[Ipv4Addr]) -> Vec<u8> {
    let ips: &[Ipv4Addr] = if q.qtype == TYPE_A && q.qclass == CLASS_IN { ips } else { &[] };
    let mut r = Vec::with_capacity(q.end + ips.len() * 16);
    r.extend_from_slice(&q.id.to_be_bytes());
    // QR + AA, keep RD from the query, RA.
    r.push(0x84 | (query[2] & 0x01));
    r.push(0x80);
    r.extend_from_slice(&[0, 1]);
    r.extend_from_slice(&(ips.len() as u16).to_be_bytes());
    r.extend_from_slice(&[0, 0, 0, 0]);
    r.extend_from_slice(&query[12..q.end]);
    for ip in ips {
        r.extend_from_slice(&[0xc0, 0x0c]); // pointer to the question name
        r.extend_from_slice(&TYPE_A.to_be_bytes());
        r.extend_from_slice(&CLASS_IN.to_be_bytes());
        r.extend_from_slice(&600u32.to_be_bytes());
        r.extend_from_slice(&4u16.to_be_bytes());
        r.extend_from_slice(&ip.octets());
    }
    r
}

/// SERVFAIL for a query that could not be forwarded.
pub fn servfail(query: &[u8], q: &Question) -> Vec<u8> {
    let mut r = answer(query, q, &[]);
    r[3] = 0x82;
    r
}

/// Does a lookup of `name` mean the container (`.` suffixes of the network
/// name are accepted too, e.g. `server.gitea_default`)?
pub fn matches(name: &str, network: &str, candidates: &[String]) -> bool {
    let base = name.strip_suffix(&format!(".{}", network.to_ascii_lowercase())).unwrap_or(name);
    candidates.iter().any(|c| c.eq_ignore_ascii_case(base))
}

#[cfg(target_os = "linux")]
pub mod server {
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
    use std::sync::Arc;
    use std::time::Duration;

    use pios_core::{info, warn};

    use super::*;

    /// Upstream resolvers: the host's, minus loopback addresses.
    pub fn upstreams() -> Vec<Ipv4Addr> {
        let text = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
        let mut v: Vec<Ipv4Addr> = text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("nameserver"))
            .filter_map(|s| s.trim().parse().ok())
            .filter(|ip: &Ipv4Addr| !ip.is_loopback())
            .collect();
        if v.is_empty() {
            v.push(Ipv4Addr::new(8, 8, 8, 8));
        }
        v
    }

    fn forward(query: &[u8]) -> Option<Vec<u8>> {
        let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
        sock.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        let mut buf = [0u8; 4096];
        for up in upstreams() {
            if sock.send_to(query, (up, 53)).is_err() {
                continue;
            }
            if let Ok((n, from)) = sock.recv_from(&mut buf) {
                if from.ip() == std::net::IpAddr::V4(up) && n >= 2 && buf[..2] == query[..2] {
                    return Some(buf[..n].to_vec());
                }
            }
        }
        None
    }

    /// Serve DNS on `gateway:53` until the socket fails (network removed).
    /// `lookup(name)` returns the addresses of matching containers.
    pub fn serve(gateway: Ipv4Addr, network: String, lookup: Arc<dyn Fn(&str) -> Vec<Ipv4Addr> + Send + Sync>) {
        let sock = match UdpSocket::bind((gateway, 53)) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                warn!("engine", "dns on {gateway}:53 ({network}): {e}");
                return;
            }
        };
        info!("engine", "dns for network {network} on {gateway}:53");
        let mut buf = [0u8; 4096];
        loop {
            let (n, peer): (usize, SocketAddr) = match sock.recv_from(&mut buf) {
                Ok(x) => x,
                Err(_) => return,
            };
            let query = buf[..n].to_vec();
            let Some(q) = parse_query(&query) else { continue };
            let ips = lookup(&q.name);
            if !ips.is_empty() {
                let _ = sock.send_to(&answer(&query, &q, &ips), peer);
                continue;
            }
            let sock = sock.clone();
            std::thread::spawn(move || {
                let reply = forward(&query).unwrap_or_else(|| servfail(&query, &q));
                let _ = sock.send_to(&reply, peer);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut m = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for l in name.split('.') {
            m.push(l.len() as u8);
            m.extend_from_slice(l.as_bytes());
        }
        m.push(0);
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&CLASS_IN.to_be_bytes());
        m
    }

    #[test]
    fn answers_a_queries() {
        let m = query("Server", TYPE_A);
        let q = parse_query(&m).unwrap();
        assert_eq!((q.id, q.name.as_str(), q.qtype), (0x1234, "server", TYPE_A));
        let r = answer(&m, &q, &[Ipv4Addr::new(172, 18, 0, 2)]);
        assert_eq!(&r[..2], &[0x12, 0x34]);
        assert_eq!(r[2] & 0x80, 0x80, "response bit");
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1, "one answer");
        assert_eq!(&r[r.len() - 4..], &[172, 18, 0, 2]);
        // AAAA for a known name: NOERROR, no answers.
        let m6 = query("server", TYPE_AAAA);
        let r6 = answer(&m6, &parse_query(&m6).unwrap(), &[Ipv4Addr::new(172, 18, 0, 2)]);
        assert_eq!(u16::from_be_bytes([r6[6], r6[7]]), 0);
        assert_eq!(r6[3] & 0x0f, 0);
        assert_eq!(servfail(&m, &q)[3] & 0x0f, 2);
    }

    #[test]
    fn rejects_responses_and_matches_names() {
        let mut m = query("x", TYPE_A);
        m[2] |= 0x80;
        assert!(parse_query(&m).is_none());
        let c = vec!["server".to_string(), "gitea-server-1".to_string()];
        assert!(matches("server", "gitea_default", &c));
        assert!(matches("server.gitea_default", "gitea_default", &c));
        assert!(!matches("db", "gitea_default", &c));
    }
}
