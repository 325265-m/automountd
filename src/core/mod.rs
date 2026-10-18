//! Pure logic layer: configuration, errors, data model, naming, policy, filesystem and mount table
//! parsing.
//!
//! This layer never executes system commands directly (commands are injected through
//! `crate::system::exec`), so the full unit test suite can run on a non-Linux development machine.

pub mod config;
pub mod error;
pub mod fstypes;
pub mod model;
pub mod mountinfo;
pub mod naming;
pub mod policy;

#[doc(hidden)]
pub mod testutil;
