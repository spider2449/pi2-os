//! Host side networking: the `pios0` bridge, NAT for bridged containers and
//! DNAT rules for published ports (iptables), container cgroups.

use std::fs::{File, OpenOptions};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::Command;

use pios_core::error::IoContext;
use pios_core::{warn, Error, Result};

use crate::netlink;
use crate::state::{HostConfig, PortMap, Proto};

pub const BRIDGE: &str = "pios0";
pub const SUBNET: Ipv4Addr = Ipv4Addr::new(172, 17, 0, 0);
pub const PREFIX: u8 = 16;
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(172, 17, 0, 1);
/// nat table chain holding the published port rules.
const CHAIN: &str = "PIOS";

fn iptables(args: &[&str]) -> Result<()> {
    let out = Command::new("iptables").arg("-w").args(args).output().map_err(|e| Error::msg(format!("iptables: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::msg(format!("iptables {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())))
    }
}

/// Append a rule unless it is already there (`-C` probes).
fn ensure(table: &str, chain: &str, rule: &[&str]) -> Result<()> {
    let mut check = vec!["-t", table, "-C", chain];
    check.extend_from_slice(rule);
    if iptables(&check).is_ok() {
        return Ok(());
    }
    let mut add = vec!["-t", table, "-A", chain];
    add.extend_from_slice(rule);
    iptables(&add)
}

/// Default bridge, the port chain (emptied: no container runs yet) and NAT.
pub fn setup() -> Result<()> {
    let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
    let _ = iptables(&["-t", "nat", "-N", CHAIN]);
    iptables(&["-t", "nat", "-F", CHAIN])?;
    ensure("nat", "PREROUTING", &["-m", "addrtype", "--dst-type", "LOCAL", "-j", CHAIN])?;
    ensure("nat", "OUTPUT", &["!", "-d", "127.0.0.0/8", "-m", "addrtype", "--dst-type", "LOCAL", "-j", CHAIN])?;
    setup_bridge(BRIDGE, GATEWAY, SUBNET, PREFIX)
}

/// A bridge with its gateway address, NAT for its subnet and forwarding.
pub fn setup_bridge(bridge: &str, gateway: Ipv4Addr, subnet: Ipv4Addr, prefix: u8) -> Result<()> {
    if !netlink::exists(bridge) {
        netlink::create_bridge(bridge).map_err(|e| Error::msg(format!("create bridge {bridge}: {e}")))?;
    }
    netlink::add_address(bridge, gateway, prefix).map_err(|e| Error::msg(format!("address {bridge}: {e}")))?;
    netlink::set_up(bridge).map_err(|e| Error::msg(format!("{bridge} up: {e}")))?;
    let net = format!("{subnet}/{prefix}");
    ensure("nat", "POSTROUTING", &["-s", &net, "!", "-o", bridge, "-j", "MASQUERADE"])?;
    // Accept forwarding for the bridge even if something set FORWARD to DROP.
    ensure("filter", "FORWARD", &["-i", bridge, "-j", "ACCEPT"])?;
    ensure("filter", "FORWARD", &["-o", bridge, "-j", "ACCEPT"])?;
    Ok(())
}

/// Remove a user-defined network's bridge and rules.
pub fn teardown_bridge(bridge: &str, subnet: Ipv4Addr, prefix: u8) {
    let net = format!("{subnet}/{prefix}");
    let _ = iptables(&["-t", "nat", "-D", "POSTROUTING", "-s", &net, "!", "-o", bridge, "-j", "MASQUERADE"]);
    let _ = iptables(&["-t", "filter", "-D", "FORWARD", "-i", bridge, "-j", "ACCEPT"]);
    let _ = iptables(&["-t", "filter", "-D", "FORWARD", "-o", bridge, "-j", "ACCEPT"]);
    if netlink::exists(bridge) {
        let _ = netlink::delete(bridge);
    }
}

/// Host side veth name for a container (15 characters at most).
pub fn veth_name(id: &str) -> String {
    format!("veth{}", &id[..11.min(id.len())])
}

/// veth pair into the network namespace of `pid`, host end on the bridge.
pub fn attach(id: &str, pid: u32, bridge: &str) -> Result<()> {
    let host = veth_name(id);
    if netlink::exists(&host) {
        let _ = netlink::delete(&host);
    }
    netlink::create_veth(&host, "eth0", pid).map_err(|e| Error::msg(format!("veth {host}: {e}")))?;
    netlink::set_master(&host, bridge).map_err(|e| Error::msg(format!("{host} -> {bridge}: {e}")))?;
    netlink::set_up(&host).map_err(|e| Error::msg(format!("{host} up: {e}")))
}

/// The veth disappears with the namespace; this only covers leftovers.
pub fn detach(id: &str) {
    let host = veth_name(id);
    if netlink::exists(&host) {
        let _ = netlink::delete(&host);
    }
}

fn port_rule(p: &PortMap, ip: Ipv4Addr, bridge: &str) -> Vec<String> {
    let mut r = Vec::new();
    if let Some(h) = p.host_ip {
        r.extend(["-d".to_string(), format!("{h}/32")]);
    }
    r.extend(
        ["!", "-i", bridge, "-p", p.proto.name(), "--dport"]
            .iter()
            .map(|s| s.to_string())
            .chain([p.host_port.to_string(), "-j".into(), "DNAT".into(), "--to-destination".into(), format!("{ip}:{}", p.container_port)]),
    );
    r
}

pub fn publish(ports: &[PortMap], ip: Ipv4Addr, bridge: &str) -> Result<()> {
    for p in ports {
        let rule = port_rule(p, ip, bridge);
        let mut args: Vec<&str> = vec!["-t", "nat", "-A", CHAIN];
        args.extend(rule.iter().map(String::as_str));
        iptables(&args)?;
    }
    Ok(())
}

pub fn unpublish(ports: &[PortMap], ip: Ipv4Addr, bridge: &str) {
    for p in ports {
        let rule = port_rule(p, ip, bridge);
        let mut args: Vec<&str> = vec!["-t", "nat", "-D", CHAIN];
        args.extend(rule.iter().map(String::as_str));
        if let Err(e) = iptables(&args) {
            warn!("engine", "{e}");
        }
    }
}

/// An unused host port, chosen by the kernel (`-p 80` without a host port).
pub fn free_port(proto: Proto, taken: &[u16]) -> Result<u16> {
    for _ in 0..16 {
        let port = match proto {
            Proto::Tcp => std::net::TcpListener::bind(("0.0.0.0", 0)).and_then(|l| l.local_addr())?.port(),
            Proto::Udp => std::net::UdpSocket::bind(("0.0.0.0", 0)).and_then(|s| s.local_addr())?.port(),
        };
        if !taken.contains(&port) {
            return Ok(port);
        }
    }
    Err(Error::msg("no free host port"))
}

/// cgroup v2 parent of all containers (outside PiOS' service tree, which
/// may not hold processes in inner nodes).
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup/containers";

pub fn cgroup_init() -> Result<()> {
    std::fs::create_dir_all(CGROUP_ROOT).ctx(|| format!("mkdir {CGROUP_ROOT}"))?;
    for c in ["+cpu", "+cpuset", "+memory", "+io", "+pids"] {
        let _ = std::fs::write(format!("{CGROUP_ROOT}/cgroup.subtree_control"), c);
    }
    Ok(())
}

pub fn cgroup_path(id: &str) -> PathBuf {
    PathBuf::from(CGROUP_ROOT).join(id)
}

/// Create the container cgroup with its limits; returns `cgroup.procs`.
pub fn cgroup_create(id: &str, h: &HostConfig) -> Result<File> {
    let dir = cgroup_path(id);
    std::fs::create_dir_all(&dir).ctx(|| format!("mkdir {}", dir.display()))?;
    let set = |file: &str, value: String| -> Result<()> {
        std::fs::write(dir.join(file), &value).ctx(|| format!("{}/{file} <- {value}", dir.display()))
    };
    if h.memory > 0 {
        set("memory.max", h.memory.to_string())?;
        // Docker: MemorySwap is memory+swap; 0 means twice the memory, -1 unlimited.
        let swap = match h.memory_swap {
            -1 => "max".to_string(),
            0 => h.memory.to_string(),
            s if s >= h.memory => (s - h.memory).to_string(),
            _ => return Err(Error::msg("Minimum memoryswap limit should be larger than memory limit")),
        };
        let _ = set("memory.swap.max", swap);
    }
    if h.nano_cpus > 0 {
        set("cpu.max", format!("{} 100000", h.nano_cpus / 10_000))?;
    } else if h.cpu_quota > 0 {
        let period = if h.cpu_period > 0 { h.cpu_period } else { 100_000 };
        set("cpu.max", format!("{} {period}", h.cpu_quota))?;
    }
    if h.cpu_shares > 0 {
        let weight = 1 + ((h.cpu_shares.clamp(2, 262_144) - 2) * 9999) / 262_142;
        set("cpu.weight", weight.to_string())?;
    }
    if let Some(p) = h.pids_limit.filter(|p| *p > 0) {
        set("pids.max", p.to_string())?;
    }
    if !h.cpuset_cpus.is_empty() {
        set("cpuset.cpus", h.cpuset_cpus.clone())?;
    }
    OpenOptions::new().write(true).open(dir.join("cgroup.procs")).ctx(|| format!("open {}/cgroup.procs", dir.display()))
}

/// SIGKILL everything in the container cgroup (kernel 5.14+).
pub fn cgroup_kill(id: &str) {
    let _ = std::fs::write(cgroup_path(id).join("cgroup.kill"), "1");
}

/// Was the container OOM-killed? Read before removing the cgroup.
pub fn cgroup_oom_killed(id: &str) -> bool {
    std::fs::read_to_string(cgroup_path(id).join("memory.events"))
        .map(|t| t.lines().any(|l| l.strip_prefix("oom_kill ").is_some_and(|n| n.trim() != "0")))
        .unwrap_or(false)
}

pub fn cgroup_remove(id: &str) {
    let dir = cgroup_path(id);
    for _ in 0..50 {
        if !dir.exists() || std::fs::remove_dir(&dir).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    warn!("engine", "cgroup {} still busy", dir.display());
}

/// Kill and remove container cgroups left by a previous daemon instance.
pub fn cgroup_cleanup() {
    let Ok(rd) = std::fs::read_dir(CGROUP_ROOT) else { return };
    for e in rd.flatten().filter(|e| e.path().is_dir()) {
        let id = e.file_name().to_string_lossy().into_owned();
        cgroup_kill(&id);
        cgroup_remove(&id);
    }
}
