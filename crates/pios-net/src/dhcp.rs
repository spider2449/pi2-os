//! DHCPv4 message encoding/decoding (RFC 2131/2132), just enough for a
//! client: DISCOVER, REQUEST and parsing OFFER/ACK/NAK.

use std::net::Ipv4Addr;

pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;
const MAGIC: [u8; 4] = [99, 130, 83, 99];
const FIXED_LEN: usize = 236;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Discover = 1,
    Offer = 2,
    Request = 3,
    Decline = 4,
    Ack = 5,
    Nak = 6,
    Release = 7,
}

impl MessageType {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Discover,
            2 => Self::Offer,
            3 => Self::Request,
            4 => Self::Decline,
            5 => Self::Ack,
            6 => Self::Nak,
            7 => Self::Release,
            _ => return None,
        })
    }
}

/// Parsed server reply.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub kind: MessageType,
    pub xid: u32,
    pub your_ip: Ipv4Addr,
    pub server_id: Option<Ipv4Addr>,
    pub subnet_mask: Option<Ipv4Addr>,
    pub routers: Vec<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub lease_secs: Option<u32>,
    pub renew_secs: Option<u32>,
}

impl Reply {
    /// Prefix length derived from the subnet mask (defaults to /24).
    pub fn prefix(&self) -> u8 {
        self.subnet_mask.map_or(24, |m| u32::from(m).count_ones() as u8)
    }
}

/// Build a client message. `requested`/`server_id` are set for REQUEST.
pub fn build(
    kind: MessageType,
    xid: u32,
    mac: [u8; 6],
    ciaddr: Ipv4Addr,
    requested: Option<Ipv4Addr>,
    server_id: Option<Ipv4Addr>,
    hostname: &str,
) -> Vec<u8> {
    let mut p = vec![0u8; FIXED_LEN];
    p[0] = 1; // op: BOOTREQUEST
    p[1] = 1; // htype: ethernet
    p[2] = 6; // hlen
    p[4..8].copy_from_slice(&xid.to_be_bytes());
    // Ask for broadcast replies: we have no address yet to receive unicast.
    if ciaddr.is_unspecified() {
        p[10] = 0x80;
    }
    p[12..16].copy_from_slice(&ciaddr.octets());
    p[28..34].copy_from_slice(&mac);
    p.extend_from_slice(&MAGIC);
    p.extend_from_slice(&[53, 1, kind as u8]);
    // Client identifier: hardware type + MAC.
    p.extend_from_slice(&[61, 7, 1]);
    p.extend_from_slice(&mac);
    if let Some(ip) = requested {
        p.extend_from_slice(&[50, 4]);
        p.extend_from_slice(&ip.octets());
    }
    if let Some(ip) = server_id {
        p.extend_from_slice(&[54, 4]);
        p.extend_from_slice(&ip.octets());
    }
    let host = &hostname.as_bytes()[..hostname.len().min(63)];
    if !host.is_empty() {
        p.extend_from_slice(&[12, host.len() as u8]);
        p.extend_from_slice(host);
    }
    // Parameter request list: mask, router, DNS, domain, lease, T1.
    p.extend_from_slice(&[55, 6, 1, 3, 6, 15, 51, 58]);
    p.push(255);
    // Some servers ignore packets shorter than the BOOTP minimum of 300 bytes.
    if p.len() < 300 {
        p.resize(300, 0);
    }
    p
}

/// Parse a server reply; `None` for anything that is not a well-formed reply.
pub fn parse(buf: &[u8]) -> Option<Reply> {
    if buf.len() < FIXED_LEN + 4 || buf[0] != 2 || buf[FIXED_LEN..FIXED_LEN + 4] != MAGIC {
        return None;
    }
    let ip = |b: &[u8]| Ipv4Addr::new(b[0], b[1], b[2], b[3]);
    let mut r = Reply {
        kind: MessageType::Nak,
        xid: u32::from_be_bytes(buf[4..8].try_into().ok()?),
        your_ip: ip(&buf[16..20]),
        server_id: None,
        subnet_mask: None,
        routers: Vec::new(),
        dns: Vec::new(),
        lease_secs: None,
        renew_secs: None,
    };
    let mut kind = None;
    let mut i = FIXED_LEN + 4;
    while i < buf.len() {
        let code = buf[i];
        match code {
            0 => {
                i += 1;
                continue;
            }
            255 => break,
            _ => {}
        }
        let len = *buf.get(i + 1)? as usize;
        let val = buf.get(i + 2..i + 2 + len)?;
        let ips = || val.chunks_exact(4).map(ip).collect::<Vec<_>>();
        let u32v = || (len == 4).then(|| u32::from_be_bytes(val.try_into().unwrap()));
        match code {
            53 if len == 1 => kind = MessageType::from_u8(val[0]),
            54 if len == 4 => r.server_id = Some(ip(val)),
            1 if len == 4 => r.subnet_mask = Some(ip(val)),
            3 => r.routers = ips(),
            6 => r.dns = ips(),
            51 => r.lease_secs = u32v(),
            58 => r.renew_secs = u32v(),
            _ => {}
        }
        i += 2 + len;
    }
    r.kind = kind?;
    Some(r)
}

/// Wrap a UDP payload in IPv4 + UDP headers (for cooked AF_PACKET sockets,
/// which we need because the interface has no address yet).
pub fn wrap_udp(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let udp_len = 8 + payload.len();
    let total = 20 + udp_len;
    let mut p = Vec::with_capacity(total);
    p.extend_from_slice(&[0x45, 0x10]); // IPv4, IHL=5, TOS low-delay
    p.extend_from_slice(&(total as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 0]); // id, flags, fragment offset
    p.extend_from_slice(&[64, 17, 0, 0]); // TTL, UDP, checksum placeholder
    p.extend_from_slice(&src.octets());
    p.extend_from_slice(&dst.octets());
    let csum = checksum(&p[..20]);
    p[10..12].copy_from_slice(&csum.to_be_bytes());
    p.extend_from_slice(&sport.to_be_bytes());
    p.extend_from_slice(&dport.to_be_bytes());
    p.extend_from_slice(&(udp_len as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0]); // UDP checksum is optional over IPv4
    p.extend_from_slice(payload);
    p
}

/// Extract `(destination port, payload)` from an IPv4/UDP packet.
pub fn unwrap_udp(p: &[u8]) -> Option<(u16, &[u8])> {
    if p.len() < 20 || p[0] >> 4 != 4 || p[9] != 17 {
        return None;
    }
    let ihl = (p[0] & 0xf) as usize * 4;
    let udp = p.get(ihl..)?;
    if udp.len() < 8 {
        return None;
    }
    let dport = u16::from_be_bytes([udp[2], udp[3]]);
    let len = (u16::from_be_bytes([udp[4], udp[5]]) as usize).clamp(8, udp.len());
    Some((dport, &udp[8..len]))
}

fn checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = header.chunks(2).map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32).sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_roundtrip_and_checksum() {
        let pkt = wrap_udp(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, 68, 67, b"hello");
        assert_eq!(checksum(&pkt[..20]), 0, "header checksum must verify to zero");
        assert_eq!(unwrap_udp(&pkt), Some((67, &b"hello"[..])));
    }

    const MAC: [u8; 6] = [0xb8, 0x27, 0xeb, 0x01, 0x02, 0x03];

    #[test]
    fn discover_layout() {
        let p = build(MessageType::Discover, 0xdeadbeef, MAC, Ipv4Addr::UNSPECIFIED, None, None, "pios");
        assert_eq!(p.len(), 300);
        assert_eq!(&p[4..8], &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(p[10], 0x80);
        assert_eq!(&p[28..34], &MAC);
        assert_eq!(&p[236..240], &MAGIC);
        assert_eq!(&p[240..243], &[53, 1, 1]);
    }

    #[test]
    fn parses_offer() {
        // Build a fake OFFER by flipping op and adding options.
        let mut p = vec![0u8; FIXED_LEN];
        p[0] = 2;
        p[4..8].copy_from_slice(&7u32.to_be_bytes());
        p[16..20].copy_from_slice(&[192, 168, 1, 50]);
        p.extend_from_slice(&MAGIC);
        p.extend_from_slice(&[53, 1, 2, 54, 4, 192, 168, 1, 1, 1, 4, 255, 255, 255, 0]);
        p.extend_from_slice(&[3, 4, 192, 168, 1, 1, 6, 8, 1, 1, 1, 1, 8, 8, 8, 8]);
        p.extend_from_slice(&[51, 4, 0, 0, 0x0e, 0x10, 0, 255]);
        let r = parse(&p).unwrap();
        assert_eq!(r.kind, MessageType::Offer);
        assert_eq!(r.xid, 7);
        assert_eq!(r.your_ip, Ipv4Addr::new(192, 168, 1, 50));
        assert_eq!(r.prefix(), 24);
        assert_eq!(r.routers, vec![Ipv4Addr::new(192, 168, 1, 1)]);
        assert_eq!(r.dns.len(), 2);
        assert_eq!(r.lease_secs, Some(3600));
    }

    #[test]
    fn rejects_truncated() {
        assert!(parse(&[2u8; 100]).is_none());
        let mut p = vec![0u8; FIXED_LEN];
        p[0] = 2;
        p.extend_from_slice(&MAGIC);
        p.extend_from_slice(&[53, 9]); // length runs past the end
        assert!(parse(&p).is_none());
    }
}
