//! automountd — a Linux daemon for automatic disk mounting.
//!
//! Design goals:
//!
//! * No third-party runtime crates for now (the required libc FFI is declared directly); dependencies
//!   are not forbidden, but new ones must be reviewed;
//! * The daemon runs as root, mounts on hotplug through udev events, and supports mounting at boot
//!   as well as fstab persistence;
//! * Non-root users control the daemon through a Unix socket, and mount points belong to the current
//!   desktop user;
//! * System disks are refused by default, along with `nosuid,nodev`, label sanitization, and symlink
//!   checks on paths.
//!
//! The source is layered by responsibility:
//!
//! ```text
//! core/    pure logic: config, model, policy, parsing (testable on any platform)
//! system/  system interaction: devices, mounting, fstab, state, udev, sessions
//! app/     service layer: daemon, control interface, device views
//! ui/      presentation layer: CLI, TUI, terminal, logging
//! ```
//!
//! For compatibility with older code, modules are also re-exported at the crate root (for example
//! `crate::config`).

// Tests rely heavily on the `let mut config = Config::default(); config.x = ...` pattern.
#![allow(clippy::field_reassign_with_default)]

pub mod app;
pub mod core;
pub mod system;
pub mod ui;

// Legacy paths: crate::config / crate::daemon / crate::mounter ...
pub use app::{ctl, daemon, inventory};
pub use core::{config, error, fstypes, model, mountinfo, naming, policy, testutil};
pub use system::{devices, exec, fstab, mounter, notify, platform, sessions, state, udev};
pub use ui::{cli, i18n, logging, term, tui};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
