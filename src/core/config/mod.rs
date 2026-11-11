//! mod.rs
//! core::config::mod
//!
//! Path resolution, config.toml settings, and the per-host MCP client policy.
//! The path helpers in paths stay reachable as core::config::<fn> through the glob re-export.
//!

pub mod client;
mod paths;
pub mod settings;

pub use paths::*;
