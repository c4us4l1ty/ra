//! A ~100 line logger so the panel does not need the `log` crate.
//!
//! Output goes to stderr, which is journald's channel when the panel is
//! started from a systemd user unit or from the labwc/XFCE autostart file.
//! Verbosity is controlled by `RA_LOG` (`error`, `warn`, `info`, `debug`,
//! `trace`); the default is `warn`, which keeps a healthy panel silent.

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

pub const ERROR: u8 = 1;
pub const WARN: u8 = 2;
pub const INFO: u8 = 3;
pub const DEBUG: u8 = 4;
pub const TRACE: u8 = 5;

static LEVEL: AtomicU8 = AtomicU8::new(0);

/// Resolve the log level from `RA_LOG`, defaulting to `warn`.
pub fn init() {
    let level = match std::env::var("RA_LOG").unwrap_or_default().to_ascii_lowercase().as_str() {
        "off" | "none" | "0" => 0,
        "error" | "1" => ERROR,
        "info" | "3" => INFO,
        "debug" | "4" => DEBUG,
        "trace" | "5" => TRACE,
        _ => WARN,
    };
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn enabled(level: u8) -> bool {
    level <= LEVEL.load(Ordering::Relaxed)
}

/// Write one line. A panic here would take the panel down, so every failure
/// here is swallowed after a single fallback attempt.
pub fn emit(level: u8, tag: &str, args: std::fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    // Always write to stderr, unbuffered and line-at-a-time: a panel that is
    // killed by the session manager must not lose its last log line.
    let _ = writeln!(std::io::stderr(), "[{tag}] {args}");
    let _ = std::io::stderr().flush();
}

#[macro_export]
macro_rules! log_at {
    ($lvl:expr, $tag:expr, $($arg:tt)*) => {
        $crate::logging::emit($lvl, $tag, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log_at!($crate::logging::ERROR, "ra", $($arg)*) };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log_at!($crate::logging::WARN, "ra", $($arg)*) };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log_at!($crate::logging::INFO, "ra", $($arg)*) };
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log_at!($crate::logging::DEBUG, "ra", $($arg)*) };
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::log_at!($crate::logging::TRACE, "ra", $($arg)*) };
}