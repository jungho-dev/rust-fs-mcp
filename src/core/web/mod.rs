//! mod.rs
//! core::web::mod
//!
//! Web layer: the SSRF-guarded HTTP client and the dependency-free HTML converters.
//! The HTTP client items in http stay reachable as core::web::<item> through the glob re-export.
//!

pub mod html;
mod http;

pub use http::*;
