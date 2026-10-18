//! Minimal platform wrappers (Linux / other Unix / Windows test environments).
//!
//! No `libc` crate: the required C functions are declared directly and linked
//! against glibc/musl on Linux.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, Result};

/// Set when SIGINT/SIGTERM arrives.
pub static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst)
}

pub fn request_stop() {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

pub fn reset_stop() {
    STOP_REQUESTED.store(false, Ordering::SeqCst);
}

#[cfg(unix)]
mod sys {
    use std::os::raw::{c_char, c_int};

    extern "C" {
        pub fn geteuid() -> u32;
        pub fn chown(path: *const c_char, owner: u32, group: u32) -> c_int;
        pub fn isatty(fd: c_int) -> c_int;
        pub fn kill(pid: c_int, sig: c_int) -> c_int;
        pub fn signal(sig: c_int, handler: usize) -> usize;
    }

    pub const SIGINT: c_int = 2;
    pub const SIGKILL: c_int = 9;
    pub const SIGTERM: c_int = 15;
}

#[cfg(unix)]
extern "C" fn handle_signal(_signal: std::os::raw::c_int) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

/// Current effective uid; 0 on non-Unix platforms (so dry-run tests can run on Windows).
pub fn euid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { sys::geteuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

pub fn is_root() -> bool {
    euid() == 0
}

/// Change the owner of a file.
pub fn chown(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains a NUL byte")
        })?;
        let rc = unsafe { sys::chown(c_path.as_ptr(), uid, gid) };
        if rc == 0 {
            return Ok(());
        }
        Err(std::io::Error::last_os_error())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, uid, gid);
        Ok(())
    }
}

/// Whether stdout is attached to a terminal (decides whether color is emitted).
pub fn stdout_is_tty() -> bool {
    #[cfg(unix)]
    {
        unsafe { sys::isatty(1) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Install the SIGINT/SIGTERM handlers; must be called on the main thread.
pub fn install_signal_handlers() -> Result<()> {
    #[cfg(unix)]
    {
        unsafe {
            sys::signal(sys::SIGINT, handle_signal as *const () as usize);
            sys::signal(sys::SIGTERM, handle_signal as *const () as usize);
        }
    }
    Ok(())
}

/// Send SIGTERM to a process, used to stop child processes (e.g. `udevadm monitor`).
pub fn terminate_process(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        unsafe { sys::kill(pid as i32, sys::SIGTERM) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Kill a process forcefully.
pub fn kill_process(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        unsafe { sys::kill(pid as i32, sys::SIGKILL) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Whether the process still exists (a signal-0 probe needs extra FFI, so /proc is used instead).
pub fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

/// Require the current process to be root, otherwise return the shared error.
pub fn require_root(operation: &str) -> Result<()> {
    if is_root() {
        return Ok(());
    }
    Err(
        Error::permission(format!("{operation} requires root privileges")).with_hint(
            "Use sudo, or start the automountd service and run it via 'automountd mount ...'",
        ),
    )
}

/// Current time (Unix seconds).
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
