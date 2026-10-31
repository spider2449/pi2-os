//! Minimal logger: writes `pios[<module>]: message` lines to stderr, which
//! PID 1 points at the serial console. Kept dependency-free on purpose.

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

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

pub fn write(level: Level, module: &str, args: std::fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    let tag = match level {
        Level::Error => "ERROR ",
        Level::Warn => "WARN ",
        Level::Info | Level::Debug => "",
    };
    // A single write_fmt on a locked handle keeps lines from interleaving.
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "pios[{module}]: {tag}{args}");
}

#[macro_export]
macro_rules! error { ($m:expr, $($a:tt)*) => { $crate::log::write($crate::log::Level::Error, $m, format_args!($($a)*)) } }
#[macro_export]
macro_rules! warn { ($m:expr, $($a:tt)*) => { $crate::log::write($crate::log::Level::Warn, $m, format_args!($($a)*)) } }
#[macro_export]
macro_rules! info { ($m:expr, $($a:tt)*) => { $crate::log::write($crate::log::Level::Info, $m, format_args!($($a)*)) } }
#[macro_export]
macro_rules! debug { ($m:expr, $($a:tt)*) => { $crate::log::write($crate::log::Level::Debug, $m, format_args!($($a)*)) } }
