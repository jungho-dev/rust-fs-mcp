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
use serde_json::{Map, Value, json};
use std::time::Instant;

// 1. Tool dispatch ------------------------------------------------------------
pub fn dispatch_tool_call(tool_name: &str, args: Option<Value>) -> Value {
    let started = Instant::now();
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
// 2. Arg shape absorption ------------------------------------------------------
// 히스토리 로그 상 최다 서버 오류가 items[]/paths[] 래핑 누락과 아이템 키 이름 혼동이므로
// 디스패치 직전에 흡수한다: flat 단일 호출은 items/paths로 감싸고 별칭 키를 정규화.
fn absorb_arg_shape(tool_name: &str, args: Value) -> Value {
    let Value::Object(mut map) = args else {
        return args;
    };
    // items가 단일 객체로 오면 배열로 승격.
    if matches!(map.get("items"), Some(Value::Object(_))) && let Some(single) = map.remove("items") {
      map.insert("items".to_string(), Value::Array(vec![single]));
    }
    match tool_name {
        "file-read" => {
            alias_item_keys(&mut map, &[("file_path", "path")]);
            promote_single_read_item(&mut map, &["isUrl", "offset", "length"]);
        }
        "file-read-line-range" => {
            alias_item_keys(&mut map, &[("file_path", "path")]);
            promote_single_read_item(&mut map, &["start_line", "line_count"]);
        }
        "path-stat" | "dir-create" => normalize_paths_only(&mut map),
        "file-write" => wrap_flat_item(
            &mut map,
            &[("file_path", "path")],
            &["path"],
            &[
                "path",
                "content",
                "content_path",
                "content_offset",
                "content_length",
                "mode",
            ],
        ),
        "dir-list" => wrap_flat_item(
            &mut map,
            &[("file_path", "path")],
            &["path"],
            &[
                "path",
                "depth",
                "maxEntries",
                "excludePatterns",
                "includeFiles",
            ],
        ),
        "path-remove" => wrap_flat_item(&mut map, &[], &["path"], &["path", "recursive", "force"]),
        "path-copy" => wrap_flat_item(
            &mut map,
            &[("from", "source"), ("to", "destination")],
            &["source", "destination"],
            &["source", "destination", "recursive", "force"],
        ),
        "path-move" => wrap_flat_item(
            &mut map,
            &[("from", "source"), ("to", "destination")],
            &["source", "destination"],
            &["source", "destination"],
        ),
        "fs-search" => wrap_flat_item(
            &mut map,
            &[("file_path", "path")],
            &["path", "pattern"],
            &[
                "path",
                "pattern",
                "pattern_path",
                "pattern_offset",
                "pattern_length",
                "filePattern",
                "ignoreCase",
                "maxResults",
                "includeHidden",
                "noDefaultExcludes",
                "contextLines",
                "timeout_ms",
            ],
        ),
        "file-edit" => wrap_flat_item(
            &mut map,
            &[("path", "file_path")],
            &["file_path", "old_string"],
            &[
                "file_path",
                "old_string",
                "old_string_path",
                "old_string_offset",
                "old_string_length",
                "new_string",
                "new_string_path",
                "new_string_offset",
                "new_string_length",
                "expected_replacements",
            ],
        ),
        "file-edit-lines" => wrap_flat_item(
            &mut map,
            &[("path", "file_path")],
            &["file_path", "start_line"],
            &[
                "file_path",
                "start_line",
                "end_line",
                "replacement",
                "replacement_path",
                "replacement_offset",
                "replacement_length",
                "after",
                "expected_lines",
            ],
        ),
        "web-extract" => wrap_flat_item(
            &mut map,
            &[],
            &["html", "path"],
            &["html", "path", "dump", "baseUrl"],
        ),
        _ => {}
    }
    Value::Object(map)
}
// 2a. 별칭 키 정규화 -----------------------------------------------------------
fn alias_keys(object: &mut Map<String, Value>, aliases: &[(&str, &str)]) {
    for (from, to) in aliases {
        if object.contains_key(*to) || !object.contains_key(*from) {
            continue;
        }
        if let Some(value) = object.remove(*from) {
            object.insert((*to).to_string(), value);
        }
    }
}
fn alias_item_keys(map: &mut Map<String, Value>, aliases: &[(&str, &str)]) {
    if aliases.is_empty() {
        return;
    }
    if let Some(Value::Array(items)) = map.get_mut("items") {
        for item in items {
            if let Value::Object(object) = item {
                alias_keys(object, aliases);
            }
        }
    }
}
// 2b. flat 단일 호출을 items:[{...}] 로 래핑 -----------------------------------
fn wrap_flat_item(
    map: &mut Map<String, Value>,
    aliases: &[(&str, &str)],
    markers: &[&str],
    item_keys: &[&str],
) {
    alias_item_keys(map, aliases);
    alias_keys(map, aliases);
    if map.contains_key("items") || !markers.iter().any(|key| map.contains_key(*key)) {
        return;
    }
    let mut item = Map::new();
    for key in item_keys {
        if let Some(value) = map.remove(*key) {
            item.insert((*key).to_string(), value);
        }
    }
    map.insert("items".to_string(), Value::Array(vec![Value::Object(item)]));
}
// 2c. 단일 path 를 read 계열 items 로 승격 --------------------------------------
fn promote_single_read_item(map: &mut Map<String, Value>, extra_keys: &[&str]) {
    if map.contains_key("paths") || map.contains_key("items") {
        return;
    }
    if !matches!(map.get("path"), Some(Value::String(_))) {
        return;
    }
    let Some(path) = map.remove("path") else {
        return;
    };
    let mut item = Map::new();
    item.insert("path".to_string(), path);
    for key in extra_keys {
        if let Some(value) = map.remove(*key) {
            item.insert((*key).to_string(), value);
        }
    }
    map.insert("items".to_string(), Value::Array(vec![Value::Object(item)]));
}
// 2d. paths 전용 도구의 items/path 흡수 ------------------------------------------
fn normalize_paths_only(map: &mut Map<String, Value>) {
    if map.contains_key("paths") {
        return;
    }
    match map.remove("items") {
        Some(Value::Array(items)) => {
            let paths = items
                .into_iter()
                .filter_map(|item| match item {
                    Value::String(text) => Some(Value::String(text)),
                    Value::Object(mut object) => match object.remove("path") {
                        Some(Value::String(text)) => Some(Value::String(text)),
                        _ => None,
                    },
                    _ => None,
                })
                .collect::<Vec<_>>();
            if !paths.is_empty() {
                map.insert("paths".to_string(), Value::Array(paths));
            }
            return;
        }
        Some(other) => {
            map.insert("items".to_string(), other);
        }
        None => {}
    }
    if !matches!(map.get("path"), Some(Value::String(_))) {
        return;
    }
    if let Some(path) = map.remove("path") {
        map.insert("paths".to_string(), json!([path]));
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
    #[test]
    fn absorbs_flat_single_item_args() {
        let listed = absorb_arg_shape("dir-list", json!({ "path": "C:/x", "depth": 1 }));
        assert_eq!(listed["items"][0]["path"], "C:/x");
        assert_eq!(listed["items"][0]["depth"], 1);
        let written = absorb_arg_shape(
            "file-write",
            json!({ "file_path": "C:/x.txt", "content": "a" }),
        );
        assert_eq!(written["items"][0]["path"], "C:/x.txt");
        let edited = absorb_arg_shape(
            "file-edit",
            json!({ "path": "C:/x.txt", "old_string": "a", "new_string": "b" }),
        );
        assert_eq!(edited["items"][0]["file_path"], "C:/x.txt");
        assert!(edited.get("path").is_none());
    }
    #[test]
    fn aliases_item_keys_inside_items() {
        let written = absorb_arg_shape(
            "file-write",
            json!({ "items": [{ "file_path": "C:/y.txt", "content": "b" }] }),
        );
        assert_eq!(written["items"][0]["path"], "C:/y.txt");
        let copied = absorb_arg_shape(
            "path-copy",
            json!({ "items": [{ "from": "C:/a", "to": "C:/b" }] }),
        );
        assert_eq!(copied["items"][0]["source"], "C:/a");
        assert_eq!(copied["items"][0]["destination"], "C:/b");
    }
    #[test]
    fn promotes_single_read_path_with_range_keys() {
        let ranged = absorb_arg_shape(
            "file-read-line-range",
            json!({ "path": "C:/a.txt", "start_line": 3, "line_count": 2 }),
        );
        assert_eq!(ranged["items"][0]["path"], "C:/a.txt");
        assert_eq!(ranged["items"][0]["start_line"], 3);
        let read = absorb_arg_shape("file-read", json!({ "path": "C:/a.txt" }));
        assert_eq!(read["items"][0]["path"], "C:/a.txt");
    }
    #[test]
    fn normalizes_paths_only_tools() {
        let stat = absorb_arg_shape(
            "path-stat",
            json!({ "items": [{ "path": "C:/a" }, "C:/b"] }),
        );
        assert_eq!(stat["paths"], json!(["C:/a", "C:/b"]));
        let created = absorb_arg_shape("dir-create", json!({ "path": "C:/new" }));
        assert_eq!(created["paths"], json!(["C:/new"]));
    }
    #[test]
    fn single_items_object_becomes_array() {
        let shaped = absorb_arg_shape(
            "file-write",
            json!({ "items": { "path": "C:/z.txt", "content": "c" } }),
        );
        assert_eq!(shaped["items"][0]["path"], "C:/z.txt");
    }
    #[test]
    fn leaves_proper_batch_args_untouched() {
        let args = json!({ "items": [{ "path": "C:/a" }, { "path": "C:/b" }], "allowMissing": true });
        let shaped = absorb_arg_shape("dir-list", args.clone());
        assert_eq!(shaped, args);
    }
}