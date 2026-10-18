//! automountd —— Linux 磁盘自动挂载守护进程。
//!
//! 设计目标：
//!
//! * 当前不引入第三方运行时 crate（直接声明所需的 libc FFI）；依赖并非禁止，新增依赖需审查；
//! * 守护进程以 root 运行，通过 udev 事件热插拔挂载，支持开机挂载与 fstab 持久化；
//! * 非 root 用户通过 Unix socket 控制守护进程，挂载点归属当前桌面用户；
//! * 默认拒绝系统盘、`nosuid,nodev`、卷标净化、路径符号链接检查。
//!
//! 源码按职责分层：
//!
//! ```text
//! core/    纯逻辑：配置、模型、策略、解析（可在任意平台测试）
//! system/  系统交互：设备、挂载、fstab、状态、udev、会话
//! app/     服务层：守护进程、控制接口、设备视图
//! ui/      表现层：CLI、TUI、终端、日志
//! ```
//!
//! 为兼容旧代码，模块同时在 crate 根路径导出（例如 `crate::config`）。

// 测试里大量使用 `let mut config = Config::default(); config.x = ...` 的写法。
#![allow(clippy::field_reassign_with_default)]

pub mod app;
pub mod core;
pub mod system;
pub mod ui;

// 兼容旧路径：crate::config / crate::daemon / crate::mounter ...
pub use app::{ctl, daemon, inventory};
pub use core::{config, error, fstypes, model, mountinfo, naming, policy, testutil};
pub use system::{devices, exec, fstab, mounter, notify, platform, sessions, state, udev};
pub use ui::{cli, i18n, logging, term, tui};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
