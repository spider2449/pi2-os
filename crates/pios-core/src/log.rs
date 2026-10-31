//! Minimal logger. Lines go to the kernel log (`/dev/kmsg`) so they appear on
//! every console (HDMI and serial) and land in the same buffer as kernel
//! messages, which is what gets saved to the boot partition. Without kmsg
//! (e.g. on a dev host) lines go to stderr. Kept dependency-free on purpose.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    /// syslog priority. Info maps to NOTICE (5) so it shows with the
    /// `loglevel=6` console setting while kernel INFO noise stays hidden.
    fn priority(self) -> u8 {
        match self {
            Level::Error => 3,
            Level::Warn => 4,
            Level::Info => 5,
            Level::Debug => 7,
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static KMSG: OnceLock<Option<File>> = OnceLock::new();

pub fn set_level(name: &str) {
    let level = match name {
        "error" => Level::Error,
        "warn" => Level::Warn,
        "debug" => Level::Debug,
        _ => Level::Info,
    };
    LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    level as u8 <= LEVEL.load(Ordering::Relaxed)
}

fn kmsg() -> Option<&'static File> {
    KMSG.get_or_init(|| OpenOptions::new().write(true).open("/dev/kmsg").ok()).as_ref()
}

/// Emit one already formatted line at `level`.
pub fn line(level: Level, text: &str) {
    if let Some(mut k) = kmsg() {
        // One write per record: kmsg treats each write as a message.
        let _ = k.write_all(format!("<{}>{text}\n", level.priority()).as_bytes());
        // PID 1's stderr is the console, which already shows kmsg. Daemons'
        // stderr feeds the supervisor ring buffer behind `pios-ctl logs`.
        if std::process::id() == 1 {
            return;
        }
    }
    let _ = writeln!(std::io::stderr().lock(), "{text}");
}

pub fn write(level: Level, module: &str, args: std::fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    let tag = match level {
        Level::Error => "ERROR ",
        Level::Warn => "WARN ",
        Level::Info | Level::Debug => "",
    };
    line(level, &format!("pios[{module}]: {tag}{args}"));
}

#[macro_export]
macro_rules! error { ($m:expr, $($a:tt)*) => { $crate::log::write($crate::log::Level::Error, $m, format_args!($($a)*)) } }
#[macro_export]
macro_rules! warn { ($m:expr, $($a:tt)*) => { $crate::log::write($crate::log::Level::Warn, $m, format_args!($($a)*)) } }
#[macro_export]
macro_rules! info { ($m:expr, $($a:tt)*) => { $crate::log::write($crate::log::Level::Info, $m, format_args!($($a)*)) } }
#[macro_export]
macro_rules! debug { ($m:expr, $($a:tt)*) => { $crate::log::write($crate::log::Level::Debug, $m, format_args!($($a)*)) } }
