//! 纯逻辑层：配置、错误、数据模型、命名、策略、文件系统与挂载表解析。
//!
//! 这一层不直接执行系统命令（命令通过 `crate::system::exec` 注入），
//! 因此可以在非 Linux 开发机上完整地跑单元测试。

pub mod config;
pub mod error;
pub mod fstypes;
pub mod model;
pub mod mountinfo;
pub mod naming;
pub mod policy;

#[doc(hidden)]
pub mod testutil;
