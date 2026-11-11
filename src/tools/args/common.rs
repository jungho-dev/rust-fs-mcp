//! common.rs
//! tools::args::common
//!
//! Argument readers and path rules shared by the fs / git / search / web tool domains.
//!

use crate::core::args_ref::read_text_slice;
use crate::core::config::ensure_path_allowed;
use serde_json::Value;
use std::path::Path;

// 1. Argument readers ---------------------------------------------------------
pub(in crate::tools) fn bool_field(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}
pub(in crate::tools) fn item_content(
    item: &Value,
    inline_key: &str,
    path_key: &str,
) -> Result<String, String> {
    if let Some(path) = item.get(path_key).and_then(Value::as_str) {
        let path = ensure_path_allowed(path)?;
        // 스키마 키는 `<inline_key>_offset/_length`. replacement 등 string/content 미포함 키도 포함.
        let offset_key = format!("{inline_key}_offset");
        let length_key = format!("{inline_key}_length");
        let offset = item.get(&offset_key).and_then(Value::as_u64).unwrap_or(0) as usize;
        let length = item
            .get(&length_key)
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        return read_text_slice(path, offset, length);
    }
    item.get(inline_key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{inline_key} or {path_key} is required"))
}
// 2. Text and path rules ------------------------------------------------------
pub(in crate::tools) fn char_byte_index(text: &str, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    text.char_indices()
        .nth(offset)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}
// 검색 루트가 기본 제외 대상 디렉터리 내부인지 판별(명시 탐색 의도는 존중). dir-list도 공유.
pub(in crate::tools) fn path_in_heavy_dir(path: &Path) -> bool {
    path.components().any(|component| {
        let text = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        text == "node_modules" || text == "target" || text == ".git"
    })
}
