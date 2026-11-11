//! mod.rs
//! tools::mod
//!
//! Registers the args / fs / git / search / sql / stats / web domain folders and exposes the single dispatch_tool_call entry point.
//! Routes tool names to handlers and applies args_path resolution plus normalize_tool_result consistently.
//! Keeps the legacy fs_tools / git_tools / search_tools / web_tools module paths as aliases.
//! Counts every known tool call through tools::stats (a single settings check when stats are off).
//!

mod args;
pub mod fs;
pub mod git;
pub mod search;
pub mod sql;
pub mod stats;
pub mod web;

pub use self::fs as fs_tools;
pub use self::git as git_tools;
pub use self::search as search_tools;
pub use self::web as web_tools;

use crate::core::args_ref::resolve_tool_args;
use crate::core::response::{RawResult, normalize_tool_result};
use crate::tools::args::arg_shape::absorb_arg_shape;
use crate::tools::fs::{
    handle_dir_create, handle_dir_list, handle_file_diff, handle_file_edit, handle_file_edit_lines,
    handle_file_read, handle_file_read_line_range, handle_file_replace, handle_file_write,
    handle_path_copy, handle_path_move, handle_path_remove, handle_path_stat,
};
use crate::tools::git::{
    handle_git_add, handle_git_amend, handle_git_commit, handle_git_diff, handle_git_show,
    handle_git_status,
};
use crate::tools::search::{handle_fs_search, handle_path_find};
use crate::tools::sql::handle_mysql_query;
use crate::tools::stats::record_tool_call;
use crate::tools::web::{handle_download_to_file, handle_web_extract, handle_web_fetch};
use serde_json::Value;
use std::time::Instant;

// 1. Tool dispatch ------------------------------------------------------------
pub fn dispatch_tool_call(tool_name: &str, args: Option<Value>) -> Value {
    let started = Instant::now();
    record_tool_call(tool_name);
    let result = match resolve_tool_args(args) {
        Ok(args) => {
            let args = absorb_arg_shape(tool_name, args);
            dispatch_resolved(tool_name, &args)
        }
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
        "path-find" => handle_path_find(args),
        "path-stat" => handle_path_stat(args),
        "file-edit" => handle_file_edit(args),
        "file-edit-lines" => handle_file_edit_lines(args),
        "file-replace" => handle_file_replace(args),
        "file-diff" => handle_file_diff(args),
        "git-add" => handle_git_add(args),
        "git-amend" => handle_git_amend(args),
        "git-commit" => handle_git_commit(args),
        "git-diff" => handle_git_diff(args),
        "git-show" => handle_git_show(args),
        "git-status" => handle_git_status(args),
        "web-fetch" => handle_web_fetch(args),
        "web-extract" => handle_web_extract(args),
        "download-to-file" => handle_download_to_file(args),
        "mysql-query" => handle_mysql_query(args),
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
