//! Control protocol between `pios-ctl` and PID 1: one JSON request line,
//! one JSON response line, over the unix socket at `paths::CTL_SOCKET`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Logs { service: String },
    Start { service: String },
    Stop { service: String },
    Restart { service: String },
    Reboot,
    Poweroff,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Error { message: String },
    Status { uptime_secs: u64, services: Vec<ServiceStatus> },
    Logs { service: String, text: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub name: String,
    pub state: String,
    pub pid: Option<i32>,
    pub restarts: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_is_stable() {
        let req = Request::Restart { service: "dockerd".into() };
        let line = serde_json::to_string(&req).unwrap();
        assert_eq!(line, r#"{"cmd":"restart","service":"dockerd"}"#);
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), req);
        assert_eq!(serde_json::to_string(&Response::Ok).unwrap(), r#"{"kind":"ok"}"#);
    }
}
