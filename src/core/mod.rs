//! mod.rs
//! core::mod
//!
//! Gateway that registers the args_ref, batch, config, external, and response submodules.
//! Groups the shared infrastructure that tool handlers depend on in one place.
//!

pub mod args_ref;
pub mod batch;
pub mod config;
pub mod external;
pub mod response;
