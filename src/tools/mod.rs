//! mod.rs
//! tools::mod
//!
//! Registers the fs / git / inspect / search tool submodules and exposes the single dispatch_tool_call entry point.
//! Routes tool names to handlers and applies args_path resolution plus normalize_tool_result consistently.
//!

pub mod fs_tools;
pub mod git_tools;
pub mod inspect_tools;
pub mod search_tools;

use crate::core::args_ref::resolve_tool_args;
use crate::core::response::{RawResult, normalize_tool_result};
use crate::tools::fs_tools::{
    handle_dir_list, handle_dir_mk, handle_file_copy, handle_file_edit, handle_file_edit_lines,
    handle_file_infos, handle_file_lines, handle_file_move, handle_file_read, handle_file_remove,
    handle_file_write,
};
use crate::tools::git_tools::{
    handle_git_add, handle_git_commit, handle_git_cwd, handle_git_diff, handle_git_show,
    handle_git_status,
};
use crate::tools::inspect_tools::handle_fs_inspect;
use crate::tools::search_tools::{
    handle_search_get, handle_search_regex, handle_search_start, handle_search_stop,
};
use serde_json::Value;
use std::time::Instant;

// 1. Tool dispatch ------------------------------------------------------------
pub fn dispatch_tool_call(tool_name: &str, args: Option<Value>) -> Value {
    let started = Instant::now();
    let result = match resolve_tool_args(args) {
        Ok(args) => dispatch_resolved(tool_name, &args),
        Err(error) => RawResult::error(error),
    };

    normalize_tool_result(tool_name, result, started.elapsed())
}

fn dispatch_resolved(tool_name: &str, args: &Value) -> RawResult {
    match tool_name {
        "file-read" => handle_file_read(args),
        "file-lines" => handle_file_lines(args),
        "file-write" => handle_file_write(args),
        "dir-mk" => handle_dir_mk(args),
        "dir-list" => handle_dir_list(args),
        "file-copy" => handle_file_copy(args),
        "file-move" => handle_file_move(args),
        "file-remove" => handle_file_remove(args),
        "search-start" => handle_search_start(args),
        "search-regex" => handle_search_regex(args),
        "search-get" => handle_search_get(args),
        "search-stop" => handle_search_stop(args),
        "file-infos" => handle_file_infos(args),
        "file-edit" => handle_file_edit(args),
        "file-edit-lines" => handle_file_edit_lines(args),
        "git-add" => handle_git_add(args),
        "git-commit" => handle_git_commit(args),
        "git-diff" => handle_git_diff(args),
        "git-cwd" => handle_git_cwd(args),
        "git-show" => handle_git_show(args),
        "git-status" => handle_git_status(args),
        "fs-inspect" => handle_fs_inspect(args),
        _ => RawResult::error(format!("Unknown tool: {tool_name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reports_unknown_tool_as_error() {
        let response = dispatch_tool_call("unknown", Some(json!({})));
        assert_eq!(response["isError"], true);
    }
}
