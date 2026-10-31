//! `pios-ctl`: talks to PID 1 over the control socket. `server` relays the
//! same protocol over mutual TLS for remote management.

#[cfg(target_os = "linux")]
pub mod server;

use pios_core::ipc::{Request, Response};

pub const USAGE: &str = "usage: pios-ctl <status|mem|logs SERVICE|start SERVICE|stop SERVICE|restart SERVICE|reboot|poweroff>";

/// Parse command line arguments (without argv[0]) into a request.
/// `mem` is served by `status` plus local /proc reads.
pub fn parse_args(args: &[String]) -> Option<Request> {
    let svc = || args.get(1).cloned();
    Some(match args.first()?.as_str() {
        "status" | "mem" => Request::Status,
        "logs" => Request::Logs { service: svc()? },
        "start" => Request::Start { service: svc()? },
        "stop" => Request::Stop { service: svc()? },
        "restart" => Request::Restart { service: svc()? },
        "reboot" => Request::Reboot,
        "poweroff" => Request::Poweroff,
        _ => return None,
    })
}

/// Render a response for the terminal. `rss` maps a pid to its RSS in KiB.
pub fn render(resp: &Response, show_mem: bool, rss: &dyn Fn(i32) -> Option<u64>) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    match resp {
        Response::Ok => out.push_str("ok\n"),
        Response::Error { message } => {
            let _ = writeln!(out, "error: {message}");
        }
        Response::Logs { text, .. } => out.push_str(text),
        Response::Status { uptime_secs, services } => {
            let _ = writeln!(out, "uptime {}h{:02}m", uptime_secs / 3600, uptime_secs / 60 % 60);
            let _ = write!(out, "{:<14} {:<9} {:>7} {:>8}", "SERVICE", "STATE", "PID", "RESTARTS");
            out.push_str(if show_mem { "   RSS(KiB)\n" } else { "\n" });
            let mut total = 0;
            for s in services {
                let pid = s.pid.map_or("-".to_string(), |p| p.to_string());
                let _ = write!(out, "{:<14} {:<9} {:>7} {:>8}", s.name, s.state, pid, s.restarts);
                if show_mem {
                    let kb = s.pid.and_then(rss).unwrap_or(0);
                    total += kb;
                    let _ = write!(out, " {kb:>10}");
                }
                out.push('\n');
            }
            if show_mem {
                let _ = writeln!(out, "{:<42} {total:>10}", "total");
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
pub fn main(args: &[String]) -> i32 {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let Some(req) = parse_args(args) else {
        eprintln!("{USAGE}");
        return 2;
    };
    let show_mem = args.first().is_some_and(|a| a == "mem");
    let result = (|| -> std::io::Result<Response> {
        let mut s = UnixStream::connect(pios_core::paths::CTL_SOCKET)?;
        serde_json::to_writer(&mut s, &req)?;
        s.write_all(b"\n")?;
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line)?;
        serde_json::from_str(&line).map_err(std::io::Error::other)
    })();
    match result {
        Ok(resp) => {
            // Not print!: a closed pipe (pios-ctl status | head) must not panic.
            let _ = std::io::stdout().write_all(render(&resp, show_mem, &rss_kib).as_bytes());
            i32::from(matches!(resp, Response::Error { .. }))
        }
        Err(e) => {
            eprintln!("pios-ctl: {e}");
            1
        }
    }
}

/// Resident set size of a process in KiB, from /proc/<pid>/status.
#[cfg(target_os = "linux")]
fn rss_kib(pid: i32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pios_core::ipc::ServiceStatus;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse_args(&args(&["status"])), Some(Request::Status));
        assert_eq!(parse_args(&args(&["restart", "dockerd"])), Some(Request::Restart { service: "dockerd".into() }));
        assert_eq!(parse_args(&args(&["logs"])), None);
        assert_eq!(parse_args(&args(&["bogus"])), None);
    }

    #[test]
    fn renders_memory_table() {
        let resp = Response::Status {
            uptime_secs: 3725,
            services: vec![
                ServiceStatus { name: "netd".into(), state: "running".into(), pid: Some(10), restarts: 0 },
                ServiceStatus { name: "ntpd".into(), state: "done".into(), pid: None, restarts: 1 },
            ],
        };
        let text = render(&resp, true, &|pid| (pid == 10).then_some(900));
        assert!(text.starts_with("uptime 1h02m"));
        assert!(text.contains("netd"));
        assert!(text.contains("900"));
    }
}
