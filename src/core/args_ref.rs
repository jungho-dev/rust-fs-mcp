//! args_ref.rs
//! core::args_ref
//!
//! Resolver for large JSON argument indirection via args_path / args_offset / args_length.
//! Reads a slice from a file so tool handlers see the same input shape as an inline argument.
//!

use serde_json::{Map, Value};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

const READ_SLICE_CHUNK: usize = 64 * 1024;

// 1. Resolve tool args reference ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
// Resolve args_path indirection, then coerce array fields that some hosts marshal as a JSON
// string. Claude Code sends items: "[{\"path\":...}]" / paths: "[...]", so batch handlers that
// read via as_array would otherwise silently drop them; Codex sends real arrays and is untouched.
pub fn resolve_tool_args(args: Option<Value>) -> Result<Value, String> {
    let resolved = resolve_args_path(args)?;
    let Value::Object(mut map) = resolved else {
        return Ok(resolved);
    };
    coerce_stringified_arrays(&mut map);
    Ok(Value::Object(map))
}

// 1a. Coerce stringified array fields ――――――――――――――――――――――――――――――――――――――――――――――――――――――――
// Only items / paths / sessionIds are schema arrays that never carry body text, so re-parsing a
// string value here cannot corrupt a genuine string field (for example file-write content). A
// value that is not valid JSON, or parses to a non-array, is left exactly as received.
fn coerce_stringified_arrays(map: &mut Map<String, Value>) {
    for key in ["items", "paths", "sessionIds"] {
        let parsed = match map.get(key) {
            Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
            _ => None,
        };
        if let Some(value @ Value::Array(_)) = parsed {
            map.insert(key.to_string(), value);
        }
    }
}

// 1b. Resolve args_path indirection ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――
fn resolve_args_path(args: Option<Value>) -> Result<Value, String> {
    let Some(Value::Object(map)) = args else {
        return Ok(args.unwrap_or(Value::Object(Map::new())));
    };
    let Some(args_path) = map.get("args_path") else {
        return Ok(Value::Object(map));
    };
    let Some(args_path) = args_path.as_str() else {
        return Err("args_path must be a string".to_string());
    };
    let offset = optional_usize(&map, "args_offset")?.unwrap_or(0);
    let length = optional_usize(&map, "args_length")?;
    let args_text = read_text_slice(args_path, offset, length)?;
    let parsed: Value = serde_json::from_str(&args_text)
        .map_err(|error| format!("args_path must contain valid JSON: {error}"))?;
    let inline: Map<String, Value> = map
        .into_iter()
        .filter(|(key, _)| key != "args_path" && key != "args_offset" && key != "args_length")
        .collect();
    if inline.is_empty() {
        return Ok(parsed);
    }
    let Value::Object(mut parsed_map) = parsed else {
        return Err(
            "args_path JSON must be an object when inline overrides are provided".to_string(),
        );
    };
    for (key, value) in inline {
        parsed_map.insert(key, value);
    }
    Ok(Value::Object(parsed_map))
}

// 2. Read text slice ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn read_text_slice(
    path: impl AsRef<Path>,
    offset: usize,
    length: Option<usize>,
) -> Result<String, String> {
    if length == Some(0) {
        return Ok(String::new());
    }

    let path = path.as_ref();
    let file =
        File::open(path).map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let mut reader = BufReader::with_capacity(READ_SLICE_CHUNK, file);
    let mut buffer = [0u8; READ_SLICE_CHUNK];
    let mut pending = Vec::new();
    let mut skip = offset;
    let mut take = length;
    let mut result = String::new();

    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        if pending.is_empty()
            && buffer[..read].is_ascii()
            && append_ascii_slice(&buffer[..read], &mut skip, &mut take, &mut result)
        {
            return Ok(result);
        }
        pending.extend_from_slice(&buffer[..read]);
        let valid_end = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => {
                return Err(format!(
                    "Failed to read {}: stream did not contain valid UTF-8",
                    path.display()
                ));
            }
        };
        let text = std::str::from_utf8(&pending[..valid_end]).unwrap();
        if append_text_slice(text, &mut skip, &mut take, &mut result) {
            return Ok(result);
        }
        let remaining = pending[valid_end..].to_vec();
        pending.clear();
        pending.extend_from_slice(&remaining);
    }

    if !pending.is_empty() {
        return Err(format!(
            "Failed to read {}: stream did not contain valid UTF-8",
            path.display()
        ));
    }
    Ok(result)
}

fn append_ascii_slice(
    bytes: &[u8],
    skip: &mut usize,
    take: &mut Option<usize>,
    result: &mut String,
) -> bool {
    let start = (*skip).min(bytes.len());
    *skip -= start;
    if *skip > 0 {
        return false;
    }

    let available = bytes.len() - start;
    let count = match take {
        Some(remaining) => {
            let count = (*remaining).min(available);
            *remaining -= count;
            count
        }
        None => available,
    };
    if count > 0 {
        result.push_str(std::str::from_utf8(&bytes[start..start + count]).unwrap());
    }

    matches!(take, Some(0))
}

fn append_text_slice(
    text: &str,
    skip: &mut usize,
    take: &mut Option<usize>,
    result: &mut String,
) -> bool {
    for ch in text.chars() {
        if *skip > 0 {
            *skip -= 1;
            continue;
        }
        if let Some(remaining) = take {
            if *remaining == 0 {
                return true;
            }
            result.push(ch);
            *remaining -= 1;
            if *remaining == 0 {
                return true;
            }
        } else {
            result.push(ch);
        }
    }
    false
}

// 3. Optional usize ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
fn optional_usize(map: &Map<String, Value>, key: &str) -> Result<Option<usize>, String> {
    match map.get(key) {
        Some(value) => value
            .as_u64()
            .map(|value| Some(value as usize))
            .ok_or_else(|| format!("{key} must be a non-negative integer")),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolves_inline_args() {
        let args = resolve_tool_args(Some(json!({"x": 1}))).unwrap();
        assert_eq!(args["x"], 1);
    }

    #[test]
    fn coerces_stringified_items_array() {
        // Claude Code marshals the items array as a JSON string; it must parse back to an array.
        let args = resolve_tool_args(Some(json!({ "items": "[{\"path\": \"a.txt\"}]" }))).unwrap();
        assert!(args["items"].is_array());
        assert_eq!(args["items"][0]["path"], "a.txt");
    }

    #[test]
    fn coerces_stringified_paths_array() {
        let args = resolve_tool_args(Some(json!({ "paths": "[\"a.txt\", \"b.txt\"]" }))).unwrap();
        assert!(args["paths"].is_array());
        assert_eq!(args["paths"][1], "b.txt");
    }

    #[test]
    fn leaves_non_json_string_field_untouched() {
        // A bare path is not valid JSON and must stay a string so no genuine value is corrupted.
        let args = resolve_tool_args(Some(json!({ "items": "C:/not/json" }))).unwrap();
        assert!(args["items"].is_string());
    }

    #[test]
    fn leaves_real_array_untouched() {
        let args = resolve_tool_args(Some(json!({ "paths": ["a", "b"] }))).unwrap();
        assert_eq!(args["paths"][0], "a");
    }

    #[test]
    fn reads_text_slice_by_char_offset() {
        let path = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-args-slice-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::write(&path, "ab한글cd").unwrap();

        let sliced = read_text_slice(&path, 2, Some(2)).unwrap();
        assert_eq!(sliced, "한글");

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reads_ascii_slice_fast_path() {
        let path = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-args-ascii-slice-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::write(&path, "abcdef").unwrap();

        let sliced = read_text_slice(&path, 2, Some(3)).unwrap();
        assert_eq!(sliced, "cde");

        fs::remove_file(path).unwrap();
    }
}
