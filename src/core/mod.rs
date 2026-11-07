//! mod.rs
//! core::mod
//!
//! Gateway that registers the args_ref, batch, client, config, encoding, external, response, settings, and web submodules.
//! Groups the shared infrastructure that tool handlers depend on in one place.
//!

pub mod args_ref;
pub mod batch;
pub mod client;
pub mod config;
pub mod encoding;
pub mod external;
pub mod response;
pub mod settings;
pub mod web;
