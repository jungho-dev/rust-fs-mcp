//! mod.rs
//! core::mod
//!
//! Gateway that registers the call, config, process, sql, text, and web domain folders.
//! Groups the shared infrastructure that tool handlers depend on in one place.
//! Each submodule stays reachable under its flat core::<name> path through the re-exports below.
//!

pub mod call;
pub mod config;
pub mod process;
pub mod sql;
pub mod text;
pub mod web;

pub use call::{args_ref, batch, response};
pub use config::{client, settings};
pub use process::{external, isolate};
pub use text::{diff, encoding};
pub use web::html;
