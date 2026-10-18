//! Minimal logging: writes to stderr; `--quiet` suppresses the info level.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
        }
    }

    pub fn parse(value: &str) -> Level {
        match value.trim().to_ascii_uppercase().as_str() {
            "ERROR" | "ERR" => Level::Error,
            "WARN" | "WARNING" => Level::Warn,
            "DEBUG" | "TRACE" => Level::Debug,
            _ => Level::Info,
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static QUIET: AtomicBool = AtomicBool::new(false);

pub fn init(level: Level, quiet: bool) {
    LEVEL.store(level as u8, Ordering::SeqCst);
    QUIET.store(quiet, Ordering::SeqCst);
}

pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::SeqCst);
}

pub fn enabled(level: Level) -> bool {
    let configured = LEVEL.load(Ordering::Relaxed);
    if (level as u8) > configured {
        return false;
    }
    if QUIET.load(Ordering::Relaxed) && level >= Level::Info {
        return false;
    }
    true
}

pub fn log(level: Level, message: &str) {
    if !enabled(level) {
        return;
    }
    eprintln!(
        "{} {:<5} {}",
        crate::term::now_string(),
        level.as_str(),
        message
    );
}

pub fn error(message: impl AsRef<str>) {
    log(Level::Error, message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    log(Level::Warn, message.as_ref());
}

pub fn info(message: impl AsRef<str>) {
    log(Level::Info, message.as_ref());
}

pub fn debug(message: impl AsRef<str>) {
    log(Level::Debug, message.as_ref());
}
