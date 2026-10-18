//! 系统交互层：进程执行、设备发现、挂载/卸载、fstab、状态、udev、会话与通知。
//!
//! Linux 专属代码（FFI、Unix socket、udev 子进程）集中在 `platform`、`udev`、
//! `mounter`、`state` 等模块，并通过 `#[cfg(unix)]` 隔离。

pub mod devices;
pub mod exec;
pub mod fstab;
pub mod mounter;
pub mod notify;
pub mod platform;
pub mod sessions;
pub mod state;
pub mod udev;
