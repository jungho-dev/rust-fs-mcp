//! mod.rs
//! core::redis::mod
//!
//! RESP2 client for redis-command: argv encoding, bounded reply parsing, and one TCP connection per configured target.
//!

mod connection;
#[cfg(test)]
pub(crate) mod fake_server;
mod resp;

pub use connection::{ConnectOptions, Connection, RedisError};
pub use resp::{Reply, ReplyLimits};
