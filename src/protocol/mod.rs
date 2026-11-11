//! mod.rs
//! protocol::mod
//!
//! Registers the catalog and jsonrpc domain folders.
//! Isolates the JSON-RPC boundary wrapping tools/list and tools/call in the protocol layer.
//! The server module stays reachable as protocol::server through the re-export below.
//!

pub mod catalog;
pub mod jsonrpc;

pub use jsonrpc::server;
