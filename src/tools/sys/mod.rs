//! mod.rs
//! tools::sys::mod
//!
//! System tool domain: the proc-list handler over the fixed System32 tasklist.exe.
//!

mod proc;

pub use proc::handle_proc_list;
