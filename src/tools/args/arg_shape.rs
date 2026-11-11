//! arg_shape.rs
//! tools::args::arg_shape
//!
//! Absorbs flat single calls, alias keys, and string-marshaled arrays before dispatch.
//!

use serde_json::{Map, Value, json};

// 1. Arg shape absorption -----------------------------------------------------
// 히스토리 로그 상 최다 서버 오류가 items[]/paths[] 래핑 누락과 아이템 키 이름 혼동이므로
// 디스패치 직전에 흡수한다: flat 단일 호출은 items/paths로 감싸고 별칭 키를 정규화.
pub(in crate::tools) fn absorb_arg_shape(tool_name: &str, args: Value) -> Value {
    let Value::Object(mut map) = args else {
        return args;
    };
    // 배열/객체 인수를 JSON 문자열로 마샬해 보내는 호출이 있어(paths:"[...]" 등) 원래 Value로 복원.
    coerce_json_string(&mut map, &["items", "paths"]);
    // items가 단일 객체로 오면 배열로 승격.
    if matches!(map.get("items"), Some(Value::Object(_)))
        && let Some(single) = map.remove("items")
    {
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
                "noDefaultExcludes",
            ],
        ),
        "path-remove" => {
            promote_paths_to_remove_items(&mut map);
            wrap_flat_item(&mut map, &[], &["path"], &["path", "recursive", "force"]);
        }
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
                "literal",
                "wordMatch",
                "multiline",
                "maxResults",
                "maxColumns",
                "includeHidden",
                "respectGitignore",
                "noDefaultExcludes",
                "contextLines",
                "encoding",
                "timeout_ms",
            ],
        ),
        "file-edit" => wrap_flat_item(
            &mut map,
            &[("path", "file_path"), ("dry_run", "dryRun")],
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
                "dryRun",
            ],
        ),
        "file-replace" => wrap_flat_item(
            &mut map,
            &[
                ("path", "file_path"),
                ("expected_replacements", "expectedReplacements"),
                ("max_replacements", "maxReplacements"),
                ("dry_run", "dryRun"),
            ],
            &["file_path", "pattern"],
            &[
                "file_path",
                "pattern",
                "pattern_path",
                "pattern_offset",
                "pattern_length",
                "replacement",
                "replacement_path",
                "replacement_offset",
                "replacement_length",
                "mode",
                "ignoreCase",
                "dotAll",
                "wordMatch",
                "maxReplacements",
                "expectedReplacements",
                "lang",
                "timeout_ms",
                "dryRun",
            ],
        ),
        "file-diff" => wrap_flat_item(
            &mut map,
            &[("file_path", "path"), ("other_path", "otherPath")],
            &["path"],
            &[
                "path",
                "otherPath",
                "content",
                "content_path",
                "content_offset",
                "content_length",
                "contextLines",
                "ignoreWhitespace",
                "algorithm",
            ],
        ),
        "path-find" => wrap_flat_item(
            &mut map,
            &[("file_path", "path")],
            &["path"],
            &[
                "path",
                "pattern",
                "glob",
                "fullPath",
                "ignoreCase",
                "type",
                "extension",
                "size",
                "changedWithin",
                "changedBefore",
                "empty",
                "minDepth",
                "maxDepth",
                "includeHidden",
                "respectGitignore",
                "noDefaultExcludes",
                "excludePatterns",
                "followLinks",
                "sortBy",
                "details",
                "maxResults",
                "timeout_ms",
            ],
        ),
        "ast-search" => wrap_flat_item(
            &mut map,
            &[("file_path", "path")],
            &["path", "pattern"],
            &[
                "path",
                "pattern",
                "lang",
                "rewrite",
                "globs",
                "strictness",
                "contextLines",
                "includeHidden",
                "respectGitignore",
                "maxResults",
                "timeout_ms",
            ],
        ),
        "file-edit-lines" => wrap_flat_item(
            &mut map,
            &[("path", "file_path"), ("dry_run", "dryRun")],
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
                "dryRun",
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
// 1a. 별칭 키 정규화 ----------------------------------------------------------
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
// 1b. flat 단일 호출을 items:[{...}] 로 래핑 ----------------------------------
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
// 1c. 단일 path 를 read 계열 items 로 승격 ------------------------------------
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
// 1d. paths 전용 도구의 items/path 흡수 ---------------------------------------
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
// 1e. path-remove paths 배열의 items 승격 -------------------------------------
// path-stat/dir-create 는 이미 단순 paths 배열을 받지만 path-remove 는 items 만 받아, 에이전트가
// paths 로 호출하면 실패한다(히스토리 확인). 공유 recursive/force 플래그를 각 항목에 접어 넣는다.
fn promote_paths_to_remove_items(map: &mut Map<String, Value>) {
    if map.contains_key("items") {
        return;
    }
    if !matches!(map.get("paths"), Some(Value::Array(_))) {
        return;
    }
    let recursive = map.get("recursive").cloned();
    let force = map.get("force").cloned();
    let Some(Value::Array(paths)) = map.remove("paths") else {
        return;
    };
    let items: Vec<Value> = paths
        .iter()
        .filter_map(Value::as_str)
        .map(|path| {
            let mut item = Map::new();
            item.insert("path".to_string(), Value::String(path.to_string()));
            if let Some(recursive) = &recursive {
                item.insert("recursive".to_string(), recursive.clone());
            }
            if let Some(force) = &force {
                item.insert("force".to_string(), force.clone());
            }
            Value::Object(item)
        })
        .collect();
    if items.is_empty() {
        return;
    }
    map.remove("recursive");
    map.remove("force");
    map.insert("items".to_string(), Value::Array(items));
}
// 1f. 문자열로 마샬된 배열/객체 인수 복원 -------------------------------------
// 에이전트가 items/paths 를 JSON 문자열로 직렬화해 보내면(스키마 오용 실패 원인) 파싱해 되돌린다.
fn coerce_json_string(map: &mut Map<String, Value>, keys: &[&str]) {
    for key in keys {
        let text = match map.get(*key) {
            Some(Value::String(text)) => text.clone(),
            _ => continue,
        };
        let trimmed = text.trim_start();
        if !(trimmed.starts_with('[') || trimmed.starts_with('{')) {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<Value>(&text)
            && (parsed.is_array() || parsed.is_object())
        {
            map.insert((*key).to_string(), parsed);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let args =
            json!({ "items": [{ "path": "C:/a" }, { "path": "C:/b" }], "allowMissing": true });
        let shaped = absorb_arg_shape("dir-list", args.clone());
        assert_eq!(shaped, args);
    }
    #[test]
    fn path_remove_accepts_simple_paths_array() {
        // path-stat/dir-create 처럼 단순 paths 배열을 받아 items 로 승격하고 공유 플래그를 접어 넣는다.
        let shaped = absorb_arg_shape(
            "path-remove",
            json!({ "paths": ["C:/a", "C:/b"], "recursive": true }),
        );
        assert_eq!(shaped["items"][0]["path"], "C:/a");
        assert_eq!(shaped["items"][1]["path"], "C:/b");
        assert_eq!(shaped["items"][0]["recursive"], true);
        assert_eq!(shaped["items"][1]["recursive"], true);
        assert!(shaped.get("paths").is_none());
        assert!(shaped.get("recursive").is_none());
    }
    #[test]
    fn coerces_json_string_encoded_arrays() {
        // 에이전트가 배열을 JSON 문자열로 보내도(스키마 오용 실패 재현) 파싱해 정상 shape 로 흡수한다.
        let removed = absorb_arg_shape("path-remove", json!({ "paths": "[\"C:/a\", \"C:/b\"]" }));
        assert_eq!(removed["items"][0]["path"], "C:/a");
        assert_eq!(removed["items"][1]["path"], "C:/b");
        let stat = absorb_arg_shape("path-stat", json!({ "paths": "[\"C:/x\"]" }));
        assert_eq!(stat["paths"][0], "C:/x");
    }
    #[test]
    fn wraps_flat_calls_for_integrated_tools() {
        let replaced = absorb_arg_shape(
            "file-replace",
            json!({ "path": "C:/a.rs", "pattern": "a", "replacement": "b", "expected_replacements": 2 }),
        );
        assert_eq!(replaced["items"][0]["file_path"], "C:/a.rs");
        assert_eq!(replaced["items"][0]["expectedReplacements"], 2);
        let found = absorb_arg_shape("path-find", json!({ "path": "C:/r", "glob": "*.rs" }));
        assert_eq!(found["items"][0]["glob"], "*.rs");
        let searched = absorb_arg_shape(
            "ast-search",
            json!({ "path": "C:/r", "pattern": "$A.unwrap()", "lang": "rust" }),
        );
        assert_eq!(searched["items"][0]["lang"], "rust");
        let diffed = absorb_arg_shape(
            "file-diff",
            json!({ "file_path": "C:/a", "other_path": "C:/b" }),
        );
        assert_eq!(diffed["items"][0]["path"], "C:/a");
        assert_eq!(diffed["items"][0]["otherPath"], "C:/b");
        let previewed = absorb_arg_shape(
            "file-edit",
            json!({ "path": "C:/x", "old_string": "a", "new_string": "b", "dryRun": true }),
        );
        assert_eq!(previewed["items"][0]["dryRun"], true);
        let snake = absorb_arg_shape(
            "file-edit-lines",
            json!({ "items": [{ "file_path": "C:/x", "start_line": 1, "dry_run": true }] }),
        );
        assert_eq!(snake["items"][0]["dryRun"], true);
        assert!(snake["items"][0].get("dry_run").is_none());
    }
    #[test]
    fn leaves_non_json_string_paths_for_handler_error() {
        // JSON 이 아닌 문자열은 파싱하지 않고 그대로 둬 핸들러가 명확히 오류를 낸다.
        let shaped = absorb_arg_shape("path-stat", json!({ "paths": "C:/single" }));
        assert_eq!(shaped["paths"], "C:/single");
    }
}
