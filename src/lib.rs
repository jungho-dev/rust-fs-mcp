//! Describe this crate or module.
//!
//! Provide the main responsibilities and usage context.

/// 1. mod ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub mod core;
pub mod protocol;
pub mod tools;

/// 2. use ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub use core::{args_ref, batch, bundled, config, response};
pub use protocol::{catalog, server};
pub use tools::{fs_tools, git_tools, inspect_tools, process_tools, search_tools};
