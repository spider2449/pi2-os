//! Interface configuration through classic ioctls (no netlink dependency)
//! and a cooked AF_PACKET socket for DHCP.

use std::ffi::CString;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use pios_core::error::IoContext;
use pios_core::Result;

const SIOCSIFFLAGS: u32 = 0x8914;
const SIOCGIFFLAGS: u32 = 0x8913;
const SIOCSIFADDR: u32 = 0x8916;
const SIOCSIFNETMASK: u32 = 0x891c;
const SIOCADDRT: u32 = 0x890b;
const RTF_UP: u16 = 0x1;
const RTF_GATEWAY: u16 = 0x2;
const ETH_P_IP: u16 = 0x0800;

/// `struct ifreq` with the union viewed as either flags or a sockaddr_in.
#[repr(C)]
struct IfReq {
    name: [u8; 16],
    data: [u8; 16],
}

/// `struct rtentry` from <linux/route.h>.
#[repr(C)]
struct RtEntry {
    pad1: libc::c_ulong,
    dst: libc::sockaddr_in,
    gateway: libc::sockaddr_in,
    genmask: libc::sockaddr_in,
    flags: libc::c_ushort,
    pad2: libc::c_short,
    pad3: libc::c_ulong,
    pad4: *mut libc::c_void,
    metric: libc::c_short,
    dev: *mut libc::c_char,
    mtu: libc::c_ulong,
    window: libc::c_ulong,
    irtt: libc::c_ushort,
}

fn sockaddr(ip: Ipv4Addr) -> libc::sockaddr_in {
    // SAFETY: sockaddr_in is plain data; zero is a valid initial value.
    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    sa.sin_addr.s_addr = u32::from_ne_bytes(ip.octets());
    sa
}

fn ifreq(name: &str) -> IfReq {
    let mut r = IfReq { name: [0; 16], data: [0; 16] };
    let n = name.len().min(15);
    r.name[..n].copy_from_slice(&name.as_bytes()[..n]);
    r
}

fn ctl_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket(2) call.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn ioctl<T>(req: u32, arg: &mut T) -> io::Result<()> {
    let sock = ctl_socket()?;
    // SAFETY: arg points to a correctly laid out struct for `req`.
    let ret = unsafe { libc::ioctl(sock.as_raw_fd(), req as _, arg as *mut T) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn set_up(name: &str) -> Result<()> {
    let mut r = ifreq(name);
    ioctl(SIOCGIFFLAGS, &mut r).ctx(|| format!("get flags {name}"))?;
    let flags = i16::from_ne_bytes([r.data[0], r.data[1]]) | (libc::IFF_UP as i16);
    r.data[..2].copy_from_slice(&flags.to_ne_bytes());
    ioctl(SIOCSIFFLAGS, &mut r).ctx(|| format!("set {name} up"))
}

fn set_sockaddr(name: &str, req: u32, ip: Ipv4Addr) -> io::Result<()> {
    let mut r = ifreq(name);
    let sa = sockaddr(ip);
    // SAFETY: sockaddr_in is exactly 16 bytes, the size of the ifreq union.
    let bytes: [u8; 16] = unsafe { std::mem::transmute(sa) };
    r.data = bytes;
    ioctl(req, &mut r)
}

pub fn set_address(name: &str, ip: Ipv4Addr, prefix: u8) -> Result<()> {
    let mask = Ipv4Addr::from(if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) });
    set_sockaddr(name, SIOCSIFADDR, ip).ctx(|| format!("set {name} address {ip}"))?;
    set_sockaddr(name, SIOCSIFNETMASK, mask).ctx(|| format!("set {name} netmask {mask}"))
}

pub fn flush_address(name: &str) -> Result<()> {
    set_sockaddr(name, SIOCSIFADDR, Ipv4Addr::UNSPECIFIED).ctx(|| format!("flush {name}"))
}

pub fn add_default_route(name: &str, gateway: Ipv4Addr) -> Result<()> {
    let dev = CString::new(name).unwrap();
    let mut rt = RtEntry {
        pad1: 0,
        dst: sockaddr(Ipv4Addr::UNSPECIFIED),
        gateway: sockaddr(gateway),
        genmask: sockaddr(Ipv4Addr::UNSPECIFIED),
        flags: RTF_UP | RTF_GATEWAY,
        pad2: 0,
        pad3: 0,
        pad4: std::ptr::null_mut(),
        metric: 0,
        dev: dev.as_ptr() as *mut _,
        mtu: 0,
        window: 0,
        irtt: 0,
    };
    match ioctl(SIOCADDRT, &mut rt) {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        r => r.ctx(|| format!("add default route via {gateway}")),
    }
}

pub fn mac(name: &str) -> Result<[u8; 6]> {
    let s = pios_core::sys::read_attr(format!("/sys/class/net/{name}/address"))?;
    let mut mac = [0u8; 6];
    for (i, part) in s.split(':').take(6).enumerate() {
        mac[i] = u8::from_str_radix(part, 16).unwrap_or(0);
    }
    Ok(mac)
}

pub fn has_carrier(name: &str) -> bool {
    pios_core::sys::read_attr(format!("/sys/class/net/{name}/carrier")).is_ok_and(|v| v == "1")
}

/// Cooked packet socket bound to one interface, carrying raw IPv4 packets.
pub struct PacketSocket {
    fd: OwnedFd,
    ifindex: i32,
}

impl PacketSocket {
    pub fn open(name: &str) -> Result<Self> {
        let cname = CString::new(name).unwrap();
        // SAFETY: valid C string.
        let ifindex = unsafe { libc::if_nametoindex(cname.as_ptr()) } as i32;
        if ifindex == 0 {
            return Err(io::Error::last_os_error()).ctx(|| format!("if_nametoindex {name}"));
        }
        let proto = ETH_P_IP.to_be() as i32;
        // SAFETY: plain socket(2) call.
        let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, proto) };
        if fd < 0 {
            return Err(io::Error::last_os_error()).ctx(|| "packet socket".into());
        }
        // SAFETY: fresh descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let sll = Self::addr(ifindex, [0; 6]);
        // SAFETY: sll is a valid sockaddr_ll of the given size.
        let ret = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &sll as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_ll>() as u32,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error()).ctx(|| format!("bind packet socket to {name}"));
        }
        Ok(Self { fd, ifindex })
    }

    fn addr(ifindex: i32, mac: [u8; 6]) -> libc::sockaddr_ll {
        // SAFETY: plain data.
        let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        sll.sll_family = libc::AF_PACKET as u16;
        sll.sll_protocol = ETH_P_IP.to_be();
        sll.sll_ifindex = ifindex;
        sll.sll_halen = 6;
        sll.sll_addr[..6].copy_from_slice(&mac);
        sll
    }

    /// Send an IPv4 packet to the Ethernet broadcast address.
    pub fn send_broadcast(&self, packet: &[u8]) -> Result<()> {
        let sll = Self::addr(self.ifindex, [0xff; 6]);
        // SAFETY: buffer and address are valid for the call.
        let n = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                packet.as_ptr().cast(),
                packet.len(),
                0,
                &sll as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_ll>() as u32,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error()).ctx(|| "send dhcp".into());
        }
        Ok(())
    }

    /// Receive one packet, or `None` on timeout.
    pub fn recv(&self, buf: &mut [u8], timeout: Duration) -> Option<usize> {
        let mut pfd = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: single valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) } <= 0 {
            return None;
        }
        // SAFETY: buffer is valid for writes of its length.
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        (n > 0).then_some(n as usize)
    }
}
