//! mod.rs
//! tools::git::mod
//!
//! Collection of git tool handlers that invoke the git CLI resolved from PATH.
//! cwd / status / add / commit / amend / diff / show all preserve the structuredContent key contract.
//! Submodules: repo (runner, toplevel cache), worktree, commit, inspect.
//!

mod commit;
mod inspect;
mod repo;
mod worktree;

pub use commit::{handle_git_amend, handle_git_commit};
pub use inspect::{handle_git_diff, handle_git_show};
pub use worktree::{handle_git_add, handle_git_status};
