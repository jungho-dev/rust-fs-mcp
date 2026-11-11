//! mod.rs
//! tools::stats::mod
//!
//! Tool usage statistics: the opt-in per-day call counter and its day-file table writer.
//! The counter items stay reachable as tools::stats::<item> through the glob re-export.
//!

mod counter;

pub use counter::*;
