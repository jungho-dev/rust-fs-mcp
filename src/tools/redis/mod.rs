//! mod.rs
//! tools::redis::mod
//!
//! redis-command tool domain: the handler, its gate, the cached per-target session, and reply rendering.
//!

mod command;
mod gate;
mod render;
mod session;

pub use command::handle_redis_command;
pub use session::close_idle_redis_session;
