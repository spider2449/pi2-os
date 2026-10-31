//! `pios netd`: brings up loopback and the uplink, then keeps a DHCP lease
//! (or applies a static address and exits).

use std::fmt::Write as _;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use pios_core::config::{NetworkMode, PiosConfig};
use pios_core::{info, paths, warn, Error, Result};

use crate::dhcp::{self, MessageType, Reply};
use crate::iface::{self, PacketSocket};

pub fn run(cfg: &PiosConfig) -> Result<()> {
    iface::set_up("lo")?;
    iface::set_address("lo", Ipv4Addr::LOCALHOST, 8)?;
    let net = &cfg.network;
    let ifname = &resolve_interface(&net.interface);
    wait_for_link(ifname, net.carrier_timeout_secs);
    match net.mode {
        NetworkMode::Static => {
            let s = net.static_ip.as_ref().ok_or_else(|| Error::Config("static_ip missing".into()))?;
            iface::set_address(ifname, s.address, s.prefix)?;
            if let Some(gw) = s.gateway {
                iface::add_default_route(ifname, gw)?;
            }
            write_resolv(&net.dns, &[])?;
            mark_ready()?;
            info!("netd", "{ifname} {}/{} (static)", s.address, s.prefix);
            Ok(())
        }
        NetworkMode::Dhcp => dhcp_loop(cfg, ifname),
    }
}

/// Interfaces that are never the uplink.
fn is_virtual(name: &str) -> bool {
    ["lo", "docker", "veth", "br-", "zt", "tun", "wg", "dummy"].iter().any(|p| name.starts_with(p))
}

/// Resolve "auto" to the first physical interface, waiting for the USB NIC
/// (smsc95xx) to enumerate if necessary.
fn resolve_interface(configured: &str) -> String {
    if configured != "auto" {
        return configured.to_string();
    }
    loop {
        let mut names: Vec<String> = std::fs::read_dir("/sys/class/net")
            .map(|d| d.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
            .unwrap_or_default();
        names.retain(|n| !is_virtual(n));
        names.sort();
        if let Some(n) = names.into_iter().next() {
            return n;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_for_link(ifname: &str, carrier_timeout_secs: u64) {
    let mut warned = false;
    loop {
        if iface::set_up(ifname).is_ok() {
            let deadline = Instant::now() + Duration::from_secs(carrier_timeout_secs);
            while Instant::now() < deadline {
                if iface::has_carrier(ifname) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        if !warned {
            warn!("netd", "waiting for {ifname} link");
            warned = true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn mark_ready() -> Result<()> {
    std::fs::write(paths::NET_READY, b"")?;
    Ok(())
}

fn write_resolv(extra: &[Ipv4Addr], learned: &[Ipv4Addr]) -> Result<()> {
    let mut s = String::new();
    for ip in learned.iter().chain(extra).take(3) {
        let _ = writeln!(s, "nameserver {ip}");
    }
    if s.is_empty() {
        s.push_str("nameserver 1.1.1.1\n");
    }
    s.push_str("options timeout:2 attempts:2\n");
    let tmp = format!("{}.tmp", paths::RESOLV_CONF);
    std::fs::write(&tmp, s)?;
    std::fs::rename(tmp, paths::RESOLV_CONF)?;
    Ok(())
}

fn xid() -> u32 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (t.subsec_nanos() ^ std::process::id().rotate_left(16)) | 1
}

struct Client<'a> {
    cfg: &'a PiosConfig,
    sock: PacketSocket,
    mac: [u8; 6],
}

impl Client<'_> {
    /// Send `msg` and wait for a matching reply of one of `want` kinds.
    fn exchange(&self, msg: &[u8], xid: u32, src: Ipv4Addr, want: &[MessageType], wait: Duration) -> Option<Reply> {
        let pkt = dhcp::wrap_udp(src, Ipv4Addr::BROADCAST, dhcp::CLIENT_PORT, dhcp::SERVER_PORT, msg);
        if let Err(e) = self.sock.send_broadcast(&pkt) {
            warn!("netd", "{e}");
            return None;
        }
        let deadline = Instant::now() + wait;
        let mut buf = [0u8; 1600];
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let n = self.sock.recv(&mut buf, left)?;
            let Some((port, payload)) = dhcp::unwrap_udp(&buf[..n]) else { continue };
            if port != dhcp::CLIENT_PORT {
                continue;
            }
            if let Some(r) = dhcp::parse(payload) {
                if r.xid == xid && (want.contains(&r.kind) || r.kind == MessageType::Nak) {
                    return Some(r);
                }
            }
        }
        None
    }

    /// Full DISCOVER → OFFER → REQUEST → ACK handshake, retried forever.
    fn acquire(&self) -> Reply {
        let mut wait = Duration::from_secs(2);
        loop {
            let id = xid();
            let host = &self.cfg.hostname;
            let discover = dhcp::build(MessageType::Discover, id, self.mac, Ipv4Addr::UNSPECIFIED, None, None, host);
            if let Some(offer) = self.exchange(&discover, id, Ipv4Addr::UNSPECIFIED, &[MessageType::Offer], wait) {
                if offer.kind == MessageType::Offer {
                    let req = dhcp::build(
                        MessageType::Request,
                        id,
                        self.mac,
                        Ipv4Addr::UNSPECIFIED,
                        Some(offer.your_ip),
                        offer.server_id,
                        host,
                    );
                    if let Some(ack) = self.exchange(&req, id, Ipv4Addr::UNSPECIFIED, &[MessageType::Ack], wait) {
                        if ack.kind == MessageType::Ack {
                            return ack;
                        }
                    }
                }
            }
            wait = (wait * 2).min(Duration::from_secs(32));
        }
    }

    /// Renew an existing lease; `None` on timeout, a NAK reply forces rediscovery.
    fn renew(&self, ip: Ipv4Addr) -> Option<Reply> {
        let id = xid();
        let req = dhcp::build(MessageType::Request, id, self.mac, ip, None, None, &self.cfg.hostname);
        self.exchange(&req, id, ip, &[MessageType::Ack], Duration::from_secs(5))
    }
}

fn apply(ifname: &str, lease: &Reply, extra_dns: &[Ipv4Addr]) -> Result<()> {
    iface::set_address(ifname, lease.your_ip, lease.prefix())?;
    if let Some(&gw) = lease.routers.first() {
        iface::add_default_route(ifname, gw)?;
    }
    write_resolv(extra_dns, &lease.dns)?;
    info!(
        "netd",
        "{ifname} {}/{} gw {} lease {}s",
        lease.your_ip,
        lease.prefix(),
        lease.routers.first().map_or("none".to_string(), |g| g.to_string()),
        lease.lease_secs.unwrap_or(0)
    );
    Ok(())
}

fn dhcp_loop(cfg: &PiosConfig, ifname: &str) -> Result<()> {
    let client = Client { cfg, sock: PacketSocket::open(ifname)?, mac: iface::mac(ifname)? };
    loop {
        let mut lease = client.acquire();
        apply(ifname, &lease, &cfg.network.dns)?;
        mark_ready()?;
        // Renew at T1 until the lease expires; rediscover on NAK or expiry.
        loop {
            let lease_secs = lease.lease_secs.unwrap_or(3600).max(60) as u64;
            let t1 = lease.renew_secs.map_or(lease_secs / 2, |t| t as u64).max(30);
            let expiry = Instant::now() + Duration::from_secs(lease_secs);
            std::thread::sleep(Duration::from_secs(t1));
            let renewed = loop {
                match client.renew(lease.your_ip) {
                    Some(r) if r.kind == MessageType::Ack => break Some(r),
                    Some(_) => break None, // NAK
                    None if Instant::now() + Duration::from_secs(30) < expiry => {
                        std::thread::sleep(Duration::from_secs(30));
                    }
                    None => break None,
                }
            };
            match renewed {
                Some(r) => {
                    if r.your_ip != lease.your_ip || r.routers != lease.routers {
                        apply(ifname, &r, &cfg.network.dns)?;
                    }
                    lease = r;
                }
                None => {
                    warn!("netd", "lease lost, restarting discovery");
                    let _ = iface::flush_address(ifname);
                    break;
                }
            }
        }
    }
}
