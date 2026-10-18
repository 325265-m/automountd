//! System interaction layer: process execution, device discovery, mount/unmount,
//! fstab, state, udev, sessions and notifications.
//!
//! Linux-only code (FFI, Unix sockets, udev child processes) is concentrated in
//! `platform`, `udev`, `mounter`, `state` and other modules, isolated behind
//! `#[cfg(unix)]`.

pub mod devices;
pub mod exec;
pub mod fstab;
pub mod mounter;
pub mod notify;
pub mod platform;
pub mod sessions;
pub mod state;
pub mod udev;
