//! automountd —— Linux 磁盘自动挂载守护进程。
//!
//! 设计目标：
//!
//! * 只依赖 Rust 标准库（直接声明所需的 libc FFI），便于离线构建与审计；
//! * 守护进程以 root 运行，通过 udev 事件热插拔挂载，支持开机挂载与 fstab 持久化；
//! * 非 root 用户通过 Unix socket 控制守护进程，挂载点归属当前桌面用户；
//! * 默认拒绝系统盘、`nosuid,nodev`、卷标净化、路径符号链接检查。
//!
//! 模块分层：纯逻辑（config/naming/policy/mountinfo 解析）不触碰系统状态，
//! 系统操作集中在 `mounter`、`state`、`platform`，便于测试。

pub mod cli;
pub mod config;
pub mod ctl;
pub mod daemon;
pub mod devices;
pub mod error;
pub mod exec;
pub mod fstab;
pub mod fstypes;
pub mod inventory;
pub mod logging;
pub mod model;
pub mod mountinfo;
pub mod mounter;
pub mod naming;
pub mod notify;
pub mod platform;
pub mod policy;
pub mod sessions;
pub mod state;
pub mod term;
pub mod tui;
pub mod udev;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[doc(hidden)]
pub mod testutil;
