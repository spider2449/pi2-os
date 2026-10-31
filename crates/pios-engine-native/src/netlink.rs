//! Minimal rtnetlink client: create bridges and veth pairs, move links into
//! network namespaces, set addresses, routes and link state.

use std::ffi::CString;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_NEWADDR: u16 = 20;
const RTM_NEWROUTE: u16 = 24;
const NLMSG_ERROR: u16 = 2;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_MASTER: u16 = 10;
const IFLA_LINKINFO: u16 = 18;
const IFLA_NET_NS_PID: u16 = 19;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const VETH_INFO_PEER: u16 = 1;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_BROADCAST: u16 = 4;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;

/// Message builder: header placeholder, fixed struct, then attributes.
struct Msg {
    buf: Vec<u8>,
    nests: Vec<usize>,
}

impl Msg {
    fn new(kind: u16, flags: u16) -> Self {
        let mut buf = vec![0u8; 16];
        buf[4..6].copy_from_slice(&kind.to_ne_bytes());
        buf[6..8].copy_from_slice(&(flags | NLM_F_REQUEST | NLM_F_ACK).to_ne_bytes());
        buf[8..12].copy_from_slice(&1u32.to_ne_bytes());
        Self { buf, nests: Vec::new() }
    }

    fn raw(&mut self, data: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(data);
        self.pad();
        self
    }

    fn pad(&mut self) {
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
    }

    fn attr(&mut self, kind: u16, data: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(&((4 + data.len()) as u16).to_ne_bytes());
        self.buf.extend_from_slice(&kind.to_ne_bytes());
        self.buf.extend_from_slice(data);
        self.pad();
        self
    }

    fn attr_str(&mut self, kind: u16, s: &str) -> &mut Self {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        self.attr(kind, &v)
    }

    fn begin(&mut self, kind: u16) -> &mut Self {
        self.nests.push(self.buf.len());
        self.buf.extend_from_slice(&[0, 0]);
        self.buf.extend_from_slice(&kind.to_ne_bytes());
        self
    }

    fn end(&mut self) -> &mut Self {
        let start = self.nests.pop().expect("unbalanced nest");
        let len = (self.buf.len() - start) as u16;
        self.buf[start..start + 2].copy_from_slice(&len.to_ne_bytes());
        self
    }

    fn finish(mut self) -> Vec<u8> {
        let len = self.buf.len() as u32;
        self.buf[0..4].copy_from_slice(&len.to_ne_bytes());
        self.buf
    }
}

/// struct ifinfomsg.
fn ifinfo(index: i32, flags: u32, change: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[4..8].copy_from_slice(&index.to_ne_bytes());
    b[8..12].copy_from_slice(&flags.to_ne_bytes());
    b[12..16].copy_from_slice(&change.to_ne_bytes());
    b
}

fn transact(msg: Vec<u8>) -> io::Result<()> {
    // SAFETY: plain socket(2) call.
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: zeroed sockaddr_nl addresses the kernel.
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as u16;
    // SAFETY: buffer and address are valid for the call.
    let n = unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            msg.as_ptr().cast(),
            msg.len(),
            0,
            &sa as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: buffer is valid for writes.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut off = 0usize;
        while off + 16 <= n as usize {
            let len = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
            let kind = u16::from_ne_bytes(buf[off + 4..off + 6].try_into().unwrap());
            if kind == NLMSG_ERROR && off + 20 <= n as usize {
                let err = i32::from_ne_bytes(buf[off + 16..off + 20].try_into().unwrap());
                return if err == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(-err)) };
            }
            if len < 16 {
                break;
            }
            off += (len + 3) & !3;
        }
    }
}

pub fn index(name: &str) -> io::Result<i32> {
    let c = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: valid C string.
    let i = unsafe { libc::if_nametoindex(c.as_ptr()) };
    if i == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(i as i32)
    }
}

pub fn exists(name: &str) -> bool {
    index(name).is_ok()
}

pub fn create_bridge(name: &str) -> io::Result<()> {
    let mut m = Msg::new(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL);
    m.raw(&ifinfo(0, 0, 0)).attr_str(IFLA_IFNAME, name).begin(IFLA_LINKINFO).attr_str(IFLA_INFO_KIND, "bridge").end();
    transact(m.finish())
}

/// veth pair `host` <-> `peer`, the peer created directly inside the network
/// namespace of process `peer_ns_pid`.
pub fn create_veth(host: &str, peer: &str, peer_ns_pid: u32) -> io::Result<()> {
    let mut m = Msg::new(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL);
    m.raw(&ifinfo(0, 0, 0))
        .attr_str(IFLA_IFNAME, host)
        .begin(IFLA_LINKINFO)
        .attr_str(IFLA_INFO_KIND, "veth")
        .begin(IFLA_INFO_DATA)
        .begin(VETH_INFO_PEER)
        .raw(&ifinfo(0, 0, 0))
        .attr_str(IFLA_IFNAME, peer)
        .attr(IFLA_NET_NS_PID, &peer_ns_pid.to_ne_bytes())
        .end()
        .end()
        .end();
    transact(m.finish())
}

pub fn delete(name: &str) -> io::Result<()> {
    let mut m = Msg::new(RTM_DELLINK, 0);
    m.raw(&ifinfo(index(name)?, 0, 0));
    transact(m.finish())
}

pub fn set_master(name: &str, master: &str) -> io::Result<()> {
    let mut m = Msg::new(RTM_NEWLINK, 0);
    m.raw(&ifinfo(index(name)?, 0, 0)).attr(IFLA_MASTER, &(index(master)? as u32).to_ne_bytes());
    transact(m.finish())
}

pub fn set_up(name: &str) -> io::Result<()> {
    let up = libc::IFF_UP as u32;
    let mut m = Msg::new(RTM_NEWLINK, 0);
    m.raw(&ifinfo(index(name)?, up, up));
    transact(m.finish())
}

pub fn set_mac(name: &str, mac: [u8; 6]) -> io::Result<()> {
    let mut m = Msg::new(RTM_NEWLINK, 0);
    m.raw(&ifinfo(index(name)?, 0, 0)).attr(IFLA_ADDRESS, &mac);
    transact(m.finish())
}

pub fn add_address(name: &str, ip: Ipv4Addr, prefix: u8) -> io::Result<()> {
    let mut hdr = [0u8; 8];
    hdr[0] = libc::AF_INET as u8;
    hdr[1] = prefix;
    hdr[4..8].copy_from_slice(&(index(name)? as u32).to_ne_bytes());
    let host_bits = if prefix >= 32 { 0 } else { u32::MAX >> prefix };
    let bcast = Ipv4Addr::from(u32::from(ip) | host_bits);
    let mut m = Msg::new(RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL);
    m.raw(&hdr).attr(IFA_LOCAL, &ip.octets()).attr(IFA_ADDRESS, &ip.octets()).attr(IFA_BROADCAST, &bcast.octets());
    match transact(m.finish()) {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        r => r,
    }
}

pub fn add_default_route(name: &str, gateway: Ipv4Addr) -> io::Result<()> {
    let mut hdr = [0u8; 12];
    hdr[0] = libc::AF_INET as u8;
    hdr[4] = 254; // RT_TABLE_MAIN
    hdr[5] = 3; // RTPROT_BOOT
    hdr[6] = 0; // RT_SCOPE_UNIVERSE
    hdr[7] = 1; // RTN_UNICAST
    let mut m = Msg::new(RTM_NEWROUTE, NLM_F_CREATE | NLM_F_EXCL);
    m.raw(&hdr).attr(RTA_GATEWAY, &gateway.octets()).attr(RTA_OIF, &(index(name)? as u32).to_ne_bytes());
    transact(m.finish())
}

/// Parse "aa:bb:cc:dd:ee:ff".
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = s.split(':');
    for b in &mut mac {
        *b = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}
