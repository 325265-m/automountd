//! 平台相关的最小封装（Linux / 其他 Unix / Windows 测试环境）。
//!
//! 不引入 `libc` crate：直接声明所需的 C 函数，Linux 上链接到 glibc/musl。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, Result};

/// SIGINT/SIGTERM 到达时置位。
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

/// 当前有效 uid；非 Unix 平台返回 0（便于在 Windows 上跑 dry-run 测试）。
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

/// 修改文件所有者。
pub fn chown(path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "路径包含 NUL 字节")
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

/// 标准输出是否连接终端（决定是否输出颜色）。
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

/// 安装 SIGINT/SIGTERM 处理器；必须在主线程调用。
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

/// 向进程发送 SIGTERM，用于停止子进程（如 udevadm monitor）。
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

/// 强制杀死进程。
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

/// 进程是否仍然存在（只做信号 0 检测需要额外 FFI，这里用 /proc 判断）。
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

/// 当前进程是否 root，否则返回统一错误。
pub fn require_root(operation: &str) -> Result<()> {
    if is_root() {
        return Ok(());
    }
    Err(Error::permission(format!("{operation}需要 root 权限"))
        .with_hint("请使用 sudo，或先启动 automountd 服务后通过 'automountd mount ...' 执行"))
}

/// 当前时间（Unix 秒）。
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
