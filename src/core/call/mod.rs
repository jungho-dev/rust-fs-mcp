//! mod.rs
//! core::call::mod
//!
//! Tool-call plumbing shared by every handler: args_path indirection, batch execution, request cancellation, and the response envelope.
//!

pub mod args_ref;
pub mod batch;
pub mod cancel;
pub mod response;
