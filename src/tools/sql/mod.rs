//! mod.rs
//! tools::sql::mod
//!
//! SQL tool domain: the mysql-query handler, its read-only gate, the cached session, and result rendering.
//!

mod gate;
mod query;
mod render;
mod session;

pub use query::handle_mysql_query;
pub use session::close_idle_session;
