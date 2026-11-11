//! shared.rs
//! tools::fs::shared
//!
//! Item readers, mutation touched-path extractors, and line counting shared by the fs handlers.
//!

use serde_json::{Value, json};
use std::fs;
use std::io;
use std::path::Path;

// 1. Item readers -------------------------------------------------------------
pub(super) fn read_items(args: &Value) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(paths) = args.get("paths").and_then(Value::as_array) {
        items.extend(
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(|path| json!({ "path": path })),
        );
    }
    if let Some(raw_items) = args.get("items").and_then(Value::as_array) {
        items.extend(raw_items.iter().cloned());
    }
    items
}
pub(super) fn usize_field(value: &Value, key: &str, default: usize) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default)
}
// 2. Mutation touched paths ---------------------------------------------------
// Mutation 배치의 독립성 판정에 쓰이는 touched-path 추출기(go pathTouched 계열 대응).
// 필수 키가 없으면 빈 벡터를 돌려줘 순차 폴백을 유도한다.
fn string_field_vec(item: &Value, keys: &[&str], required: &str) -> Vec<String> {
    if item.get(required).and_then(Value::as_str).is_none() {
        return Vec::new();
    }
    keys.iter()
        .filter_map(|key| item.get(*key).and_then(Value::as_str).map(str::to_string))
        .collect()
}
pub(super) fn path_touched(item: &Value) -> Vec<String> {
    string_field_vec(item, &["path"], "path")
}
pub(super) fn write_touched(item: &Value) -> Vec<String> {
    string_field_vec(item, &["path", "content_path"], "path")
}
pub(super) fn transfer_touched(item: &Value) -> Vec<String> {
    let touched = string_field_vec(item, &["source", "destination"], "source");
    if touched.len() < 2 {
        return Vec::new();
    }
    touched
}
pub(super) fn edit_touched(item: &Value) -> Vec<String> {
    string_field_vec(
        item,
        &["file_path", "old_string_path", "new_string_path"],
        "file_path",
    )
}
pub(super) fn edit_lines_touched(item: &Value) -> Vec<String> {
    string_field_vec(item, &["file_path", "replacement_path"], "file_path")
}
// 3. Parent directory and line count ------------------------------------------
pub(super) fn ensure_parent_dir(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create {}: {error}", parent.display()))
}
// 반환 윈도우(슬라이스 내용) 기준 라인 수: 개행 수 + 마지막 줄이 개행으로 끝나지 않으면 +1.
pub(super) fn window_line_count(content: &str) -> usize {
    let breaks = memchr::memchr_iter(b'\n', content.as_bytes()).count();
    breaks + usize::from(!content.is_empty() && !content.ends_with('\n'))
}
// 4. Open-first file access ---------------------------------------------------
pub(super) enum OpenFailure {
    Missing,
    Directory,
    Unreadable(io::Error),
}
// 열기 1회 판정 (stat 재오픈 제거)
pub(super) fn open_regular_file(path: &Path) -> Result<(fs::File, fs::Metadata), OpenFailure> {
    let opened = fs::File::open(path).and_then(|file| {
        let metadata = file.metadata()?;
        Ok((file, metadata))
    });
    match opened {
        Ok((_, metadata)) if metadata.is_dir() => Err(OpenFailure::Directory),
        Ok(opened) => Ok(opened),
        // 열기 실패 분류
        Err(open_error) => match fs::metadata(path) {
            Err(_) => Err(OpenFailure::Missing),
            Ok(metadata) if metadata.is_dir() => Err(OpenFailure::Directory),
            Ok(_) => Err(OpenFailure::Unreadable(open_error)),
        },
    }
}
