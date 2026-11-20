//! mod.rs
//! core::web::mod
//!
//! Web layer: the SSRF-guarded HTTP client, bot-challenge detection, fallback sources for
//! challenged pages, and the dependency-free HTML converters.
//! The http, challenge, and fallback items stay reachable as core::web::<item> through glob re-exports.
//!

mod challenge;
mod fallback;
pub mod html;
mod http;

pub use challenge::*;
pub use fallback::*;
pub use http::*;
