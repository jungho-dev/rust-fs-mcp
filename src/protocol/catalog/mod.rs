//! mod.rs
//! protocol::catalog::mod
//!
//! Tool catalog served by tools/list: names, titles, descriptions, input schemas, and annotations.
//! The catalog items in tools stay reachable as protocol::catalog::<item> through the glob re-export.
//!

mod tools;

pub use tools::*;
