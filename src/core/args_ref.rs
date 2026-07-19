//! args_ref.rs
//! core::args_ref
//!
//! Resolver for large JSON argument indirection via args_path / args_offset / args_length.
//! Reads a slice from a file so tool handlers see the same input shape as an inline argument.
//! Also absorbs host marshaling variants that send schema arrays, booleans, or numbers as JSON strings.
//!

use serde_json::{Map, Value};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

const READ_SLICE_CHUNK: usize = 64 * 1024;

// Schema-array keys that never carry body text, so re-parsing a string value cannot corrupt a
// genuine string field (for example file-write content).
const ARRAY_ARG_KEYS: [&str; 5] = ["items", "paths", "sessionIds", "requests", "filesToStage"];
// Schema-boolean keys; a "true" / "false" string is promoted, anything else stays as received.
const BOOL_ARG_KEYS: [&str; 29] = ["after", "all", "allowEmpty", "allowMissing", "amend", "check", "force", "ignoreCase", "includeFiles", "includeHidden", "includeUntracked", "initializeIfNotPresent", "isUrl", "literal", "literalSearch", "multiline", "nameOnly", "noDefaultExcludes", "noVerify", "overwrite", "quiet", "recursive", "resetAuthor", "staged", "stat", "stealth", "update", "validateGitRepo", "wordMatch"];
// Schema-number keys; only a string that fully parses as a number is promoted. Free-text keys
// whose values can look numeric (object, pattern, content, ...) are intentionally not listed.
const NUM_ARG_KEYS: [&str; 36] = ["args_length", "args_offset", "content_length", "content_offset", "contextLines", "depth", "end_line", "expected_lines", "expected_replacements", "length", "line_count", "maxBytes", "maxEntries", "maxMatches", "maxRedirects", "maxResults", "maxSnippetChars", "maxSnippets", "messageLength", "messageOffset", "new_string_length", "new_string_offset", "offset", "old_string_length", "old_string_offset", "pattern_length", "pattern_offset", "replacement_length", "replacement_offset", "start_line", "timeout", "timeoutMs", "timeout_ms", "value_length", "value_offset", "wait"];

// 1. Resolve tool args reference -----------------------------------------------------------
// Resolve args_path indirection with host-marshaling coercion on both sides. Claude Code sends
// schema arrays as JSON strings (items: "[{\"path\":...}]") and scalars as strings
// ("allowMissing": "true"), so handlers reading via as_array / as_bool / as_u64 would silently
// drop them; Codex sends real JSON values and is untouched. Coercion runs before args_path
// resolution so stringified args_offset / args_length parse, and again afterward so inline
// overrides merged from the file go through the same absorption.
pub fn resolve_tool_args(args: Option<Value>) -> Result<Value, String> {
  let args = match args {
    Some(Value::Object(mut map)) => {
      coerce_stringified_args(&mut map);
      Some(Value::Object(map))
    }
    other => other,
  };
  let resolved = resolve_args_path(args)?;
  let Value::Object(mut map) = resolved else {
    return Ok(resolved);
  };
  coerce_stringified_args(&mut map);
  Ok(Value::Object(map))
}
// 1a. Coerce stringified argument fields ---------------------------------------------------
// Keys are whitelisted per schema type; a value that does not parse exactly stays as received.
fn coerce_stringified_args(map: &mut Map<String, Value>) {
  for key in ARRAY_ARG_KEYS {
    let parsed = match map.get(key) {
      Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
      _ => None,
    };
    if let Some(value @ Value::Array(_)) = parsed {
      map.insert(key.to_string(), value);
    }
  }
  coerce_scalar_fields(map);
  for key in ["items", "requests"] {
    if let Some(Value::Array(elements)) = map.get_mut(key) {
      for element in elements {
        if let Value::Object(item) = element {
          coerce_scalar_fields(item);
        }
      }
    }
  }
}
fn coerce_scalar_fields(map: &mut Map<String, Value>) {
  for key in BOOL_ARG_KEYS {
    let promoted = match map.get(key) {
      Some(Value::String(text)) => match text.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
      },
      _ => None,
    };
    if let Some(value) = promoted {
      map.insert(key.to_string(), Value::Bool(value));
    }
  }
  for key in NUM_ARG_KEYS {
    let promoted = match map.get(key) {
      Some(Value::String(text)) => parse_json_number(text),
      _ => None,
    };
    if let Some(value) = promoted {
      map.insert(key.to_string(), Value::Number(value));
    }
  }
}
fn parse_json_number(text: &str) -> Option<serde_json::Number> {
  if let Ok(value) = text.parse::<i64>() {
    return Some(serde_json::Number::from(value));
  }
  text.parse::<f64>().ok().and_then(serde_json::Number::from_f64)
}
// 1b. Resolve args_path indirection ---------------------------------------------------------
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
  // 실사용 오류: 대상 경로/디렉터리를 args_path에 넘기는 오용. 인자 JSON 파일만 허용.
  if Path::new(args_path).is_dir() {
    return Err(format!("args_path must be a UTF-8 JSON file containing the complete tool arguments, not a target path: {args_path}. Put the target in the tool's own fields (path/items/paths)"));
  }
  let offset = optional_usize(&map, "args_offset")?.unwrap_or(0);
  let length = optional_usize(&map, "args_length")?;
  let args_text = read_text_slice(args_path, offset, length).map_err(|error| format!("{error} (args_path must point to an existing JSON file holding the full arguments; to target a file or directory use the tool's own path fields)"))?;
  let parsed: Value = serde_json::from_str(&args_text).map_err(|error| format!("args_path must contain valid JSON: {error}"))?;
  let inline: Map<String, Value> = map.into_iter().filter(|(key, _)| key != "args_path" && key != "args_offset" && key != "args_length").collect();
  if inline.is_empty() {
    return Ok(parsed);
  }
  let Value::Object(mut parsed_map) = parsed else {
    return Err("args_path JSON must be an object when inline overrides are provided".to_string());
  };
  for (key, value) in inline {
    parsed_map.insert(key, value);
  }
  Ok(Value::Object(parsed_map))
}
// 2. Read text slice -----------------------------------------------------------------------
pub fn read_text_slice(path: impl AsRef<Path>, offset: usize, length: Option<usize>) -> Result<String, String> {
  if length == Some(0) {
    return Ok(String::new());
  }
  let path = path.as_ref();
  let file = File::open(path).map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
  let mut reader = BufReader::with_capacity(READ_SLICE_CHUNK, file);
  let mut buffer = [0u8; READ_SLICE_CHUNK];
  let mut pending = Vec::new();
  let mut skip = offset;
  let mut take = length;
  let mut result = String::new();

  loop {
    let read = reader.read(&mut buffer).map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    if read == 0 {
      break;
    }
    if pending.is_empty() && buffer[..read].is_ascii() {
      // 고속 경로가 청크를 이미 소비함. take 미소진이면 다음 청크로 진행.
      if append_ascii_slice(&buffer[..read], &mut skip, &mut take, &mut result) {
        return Ok(result);
      }
      continue;
    }
    pending.extend_from_slice(&buffer[..read]);
    let valid_end = match std::str::from_utf8(&pending) {
      Ok(_) => pending.len(),
      Err(error) if error.error_len().is_none() => error.valid_up_to(),
      Err(_) => {
        return Err(format!("Failed to read {}: stream did not contain valid UTF-8", path.display()));
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
    return Err(format!("Failed to read {}: stream did not contain valid UTF-8", path.display()));
  }
  Ok(result)
}
fn append_ascii_slice(bytes: &[u8], skip: &mut usize, take: &mut Option<usize>, result: &mut String) -> bool {
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
fn append_text_slice(text: &str, skip: &mut usize, take: &mut Option<usize>, result: &mut String) -> bool {
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
    }
    else {
    	result.push(ch);
    }
  }
  false
}
// 3. Optional usize ------------------------------------------------------------------------
fn optional_usize(map: &Map<String, Value>, key: &str) -> Result<Option<usize>, String> {
  match map.get(key) {
    Some(value) => value.as_u64().map(|value| Some(value as usize)).ok_or_else(|| format!("{key} must be a non-negative integer")),
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
  fn coerces_stringified_bool_and_number() {
    let args = resolve_tool_args(Some(json!({
      "allowMissing": "true",
      "stat": "false",
      "maxResults": "10",
      "paths": ["a"]
    })))
    .unwrap();
    assert_eq!(args["allowMissing"], true);
    assert_eq!(args["stat"], false);
    assert_eq!(args["maxResults"], 10);
  }
  #[test]
  fn coerces_scalars_inside_items_elements() {
    let args = resolve_tool_args(Some(json!({
      "items": [{
        "path": "a.txt",
        "isUrl": "false",
        "offset": "2",
        "line_count": "3"
      }]
    })))
    .unwrap();
    assert_eq!(args["items"][0]["isUrl"], false);
    assert_eq!(args["items"][0]["offset"], 2);
    assert_eq!(args["items"][0]["line_count"], 3);
  }
  #[test]
  fn coerces_stringified_requests_array() {
    let args = resolve_tool_args(Some(json!({
      "requests": "[{\"op\": \"git-status\", \"path\": \".\", \"recursive\": \"true\"}]"
    })))
    .unwrap();
    assert!(args["requests"].is_array());
    assert_eq!(args["requests"][0]["recursive"], true);
  }
  #[test]
  fn leaves_free_text_and_partial_numbers_untouched() {
    // object is free text (a revision named 123456 must stay a string) and values that do
    // not parse exactly keep their original form.
    let args = resolve_tool_args(Some(json!({
      "object": "123456",
      "maxResults": "10x",
      "allowMissing": "yes"
    })))
    .unwrap();
    assert!(args["object"].is_string());
    assert!(args["maxResults"].is_string());
    assert!(args["allowMissing"].is_string());
  }
  #[test]
  fn reads_text_slice_by_char_offset() {
    let path = std::env::current_dir().unwrap().join("target").join(format!("rust-fs-mcp-args-slice-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
    fs::write(&path, "ab한글cd").unwrap();

    let sliced = read_text_slice(&path, 2, Some(2)).unwrap();
    assert_eq!(sliced, "한글");

    fs::remove_file(path).unwrap();
  }
  #[test]
  fn reads_ascii_slice_fast_path() {
    let path = std::env::current_dir().unwrap().join("target").join(format!("rust-fs-mcp-args-ascii-slice-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
    fs::write(&path, "abcdef").unwrap();

    let sliced = read_text_slice(&path, 2, Some(3)).unwrap();
    assert_eq!(sliced, "cde");

    fs::remove_file(path).unwrap();
  }
}
