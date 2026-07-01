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
pub mod web_tools;

use crate::core::args_ref::resolve_tool_args;
use crate::core::response::{RawResult, normalize_tool_result};
use crate::tools::fs_tools::{
    handle_dir_create, handle_dir_list, handle_file_edit, handle_file_edit_lines, handle_file_read,
    handle_file_read_line_range, handle_file_write, handle_path_copy, handle_path_move,
    handle_path_remove, handle_path_stat,
};
use crate::tools::git_tools::{
    handle_git_add, handle_git_amend, handle_git_commit, handle_git_diff, handle_git_set_workdir,
    handle_git_show, handle_git_status,
};
use crate::tools::inspect_tools::handle_fs_inspect;
use crate::tools::search_tools::handle_fs_search;
use crate::tools::web_tools::{
    handle_download_to_file, handle_web_extract, handle_web_fetch, handle_web_render,
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
        "file-read-line-range" => handle_file_read_line_range(args),
        "file-write" => handle_file_write(args),
        "dir-create" => handle_dir_create(args),
        "dir-list" => handle_dir_list(args),
        "path-copy" => handle_path_copy(args),
        "path-move" => handle_path_move(args),
        "path-remove" => handle_path_remove(args),
        "fs-search" => handle_fs_search(args),
        "path-stat" => handle_path_stat(args),
        "file-edit" => handle_file_edit(args),
        "file-edit-lines" => handle_file_edit_lines(args),
        "git-add" => handle_git_add(args),
        "git-amend" => handle_git_amend(args),
        "git-commit" => handle_git_commit(args),
        "git-diff" => handle_git_diff(args),
        "git-set-workdir" => handle_git_set_workdir(args),
        "git-show" => handle_git_show(args),
        "git-status" => handle_git_status(args),
        "fs-inspect" => handle_fs_inspect(args),
        "web-fetch" => handle_web_fetch(args),
        "web-render" => handle_web_render(args),
        "web-extract" => handle_web_extract(args),
        "download-to-file" => handle_download_to_file(args),
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
