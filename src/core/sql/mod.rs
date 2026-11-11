//! mod.rs
//! core::sql::mod
//!
//! SQL database layer behind the mysql-query tool: password resolution and the MySQL wire client.
//! The client is split by domain; its public items stay reachable as core::sql::<item> through the re-exports.
//!

mod connection;
mod digest;
mod error;
#[cfg(test)]
mod fake_server;
mod handshake;
mod packet;
pub mod password;
mod random;
mod rsa;
mod statement;
mod types;

pub use connection::Connection;
pub use error::MysqlError;
pub use types::{BINARY_CHARSET, ColumnMeta, ConnectOptions, FLAG_BINARY, FLAG_UNSIGNED};
pub use types::{ResultSet, RowLimits, StatementOutcome, StatementSummary, column_type};
