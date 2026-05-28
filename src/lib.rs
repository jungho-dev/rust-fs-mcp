//! lib.rs
//! lib
//!
//! Public facade that exposes core, protocol, and tools under stable paths.
//! Lets the binary, integration tests, and external callers share the same use paths.
//!

/// 1. mod ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub mod core;
pub mod protocol;
pub mod tools;

/// 2. use ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub use core::{args_ref, batch, config, external, response};
pub use protocol::{catalog, server};
pub use tools::{fs_tools, git_tools, inspect_tools, search_tools};
