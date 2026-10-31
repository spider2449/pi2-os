//! Headless diagnostics: the ACT LED shows the boot state and the kernel log
//! is saved to the FAT boot partition, readable on any PC after power-off.

use std::sync::Mutex;

use pios_core::sys::{self, write_attr};
use pios_core::warn;

const LED: &str = "/sys/class/leds/ACT";
pub const BOOT_LOG: &str = "/boot/pios-boot.log";
const PREV_LOG: &str = "/boot/pios-boot.prev.log";

#[derive(Debug, Clone, Copy)]
pub enum Led {
    /// Slow blink: booting.
    Booting,
    /// Heartbeat: engine ready.
    Ready,
    /// Fast blink: boot failed.
    Error,
}

pub fn led(state: Led) {
    let timer = |on: u32, off: u32| {
        write_attr(format!("{LED}/trigger"), "timer")?;
        write_attr(format!("{LED}/delay_on"), &on.to_string())?;
        write_attr(format!("{LED}/delay_off"), &off.to_string())
    };
    let r = match state {
        Led::Booting => timer(500, 500),
        Led::Ready => write_attr(format!("{LED}/trigger"), "heartbeat"),
        Led::Error => timer(100, 100),
    };
    if let Err(e) = r {
        pios_core::debug!("status", "led: {e}");
    }
}

/// Serialises boot-partition writes (the snapshot thread and PID 1 both save).
static SAVE: Mutex<bool> = Mutex::new(false);

/// Copy the kernel log buffer to the boot partition. The partition is only
/// writable for the duration of the write, limiting exposure to power loss.
pub fn save_boot_log(reason: &str) {
    let mut rotated = SAVE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(log) = read_kernel_log() else { return };
    let text = format!("# PiOS boot log ({reason}, uptime {} s)\n{log}", sys::uptime_secs());
    let result = sys::with_boot_writable(|| {
        if !*rotated {
            // Keep the previous boot's log once per boot.
            let _ = std::fs::rename(BOOT_LOG, PREV_LOG);
            *rotated = true;
        }
        let mut f = std::fs::File::create(BOOT_LOG)?;
        use std::io::Write;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        Ok(())
    });
    if let Err(e) = result {
        warn!("status", "boot log: {e}");
    }
}

/// Whole kernel ring buffer via syslog(2), without the `<N>` priority prefixes.
fn read_kernel_log() -> Option<String> {
    const READ_ALL: libc::c_int = 3;
    const SIZE_BUFFER: libc::c_int = 10;
    // SAFETY: SIZE_BUFFER ignores the buffer arguments.
    let size = unsafe { libc::klogctl(SIZE_BUFFER, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    // SAFETY: buf is valid for writes of its length.
    let n = unsafe { libc::klogctl(READ_ALL, buf.as_mut_ptr().cast(), buf.len() as libc::c_int) };
    if n <= 0 {
        return None;
    }
    let raw = String::from_utf8_lossy(&buf[..n as usize]);
    Some(raw.lines().map(strip_priority).collect::<Vec<_>>().join("\n") + "\n")
}

fn strip_priority(line: &str) -> &str {
    match line.strip_prefix('<').and_then(|r| r.split_once('>')) {
        Some((p, rest)) if p.bytes().all(|b| b.is_ascii_digit()) => rest,
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::strip_priority;

    #[test]
    fn strips_syslog_prefix() {
        assert_eq!(strip_priority("<5>[    7.2] pios[init]: hi"), "[    7.2] pios[init]: hi");
        assert_eq!(strip_priority("<x>keep"), "<x>keep");
        assert_eq!(strip_priority("plain"), "plain");
    }
}
