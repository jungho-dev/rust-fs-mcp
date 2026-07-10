//! response.rs
//! core::response
//!
//! Normalizes a RawResult into the public MCP envelope (content / structuredContent / _meta / isError).
//! Applies text and JSON sanitization plus duration measurement on the same path.
//!

use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

const END_TOKEN: &str = "<|endoftext|>";
const SAFE_END_TOKEN: &str = "<|endoftext |>";

#[derive(Clone, Debug)]
pub struct RawResult {
  pub content: Vec<Value>,
  pub structured: Option<Value>,
  pub is_error: bool,
  pub meta: Map<String, Value>,
}
impl RawResult {
  // 1. Text result ----------------------------------------------------------------------------
  pub fn text(text: impl Into<String>) -> Self {
    Self { content: vec![text_content(text.into())], structured: None, is_error: false, meta: Map::new() }
  }
  // 2. Structured result ---------------------------------------------------------------------
  pub fn structured(text: impl Into<String>, structured: Value) -> Self {
    Self { content: vec![text_content(text.into())], structured: Some(sanitize_json(structured)), is_error: false, meta: Map::new() }
  }
  // 3. Error result --------------------------------------------------------------------------
  pub fn error(message: impl Into<String>) -> Self {
    Self { content: vec![text_content(format!("Error: {}", message.into()))], structured: None, is_error: true, meta: Map::new() }
  }
}
// 4. Text content --------------------------------------------------------------------------
pub fn text_content(text: String) -> Value {
  json!({
    "type": "text",
    "text": sanitize_owned(text)
  })
}
// 5. Normalize tool result -----------------------------------------------------------------
// Default-on compact envelope: drops the data.text copy that duplicates data.content for
// token-sensitive clients. Opt out with RUST_FS_MCP_COMPACT=0 (or false) to restore data.text.
static COMPACT_ENVELOPE: OnceLock<bool> = OnceLock::new();

pub fn compact_enabled() -> bool {
  *COMPACT_ENVELOPE.get_or_init(|| std::env::var("RUST_FS_MCP_COMPACT").map(|value| value != "0" && value != "false").unwrap_or(true))
}
// Plain-content mode for hosts that forward only structuredContent and drop the content[] blocks.
// Claude Code serializes structuredContent as the tool_result, so body text carried there leaks
// \n / \" escapes. When the initialize handshake reports a Claude client we omit structuredContent
// and emit the plain content[] blocks, which that host forwards to the model intact.
static PLAIN_CONTENT_MODE: AtomicBool = AtomicBool::new(false);

pub fn set_plain_content_mode(plain: bool) {
  PLAIN_CONTENT_MODE.store(plain, Ordering::Relaxed);
}
fn plain_content_mode() -> bool {
  PLAIN_CONTENT_MODE.load(Ordering::Relaxed)
}
pub fn normalize_tool_result(tool_name: &str, result: RawResult, duration: Duration) -> Value {
  build_envelope(tool_name, result, duration, plain_content_mode())
}
fn build_envelope(tool_name: &str, result: RawResult, duration: Duration, plain: bool) -> Value {
  let is_error = result.is_error;
  let content = normalize_content(result.content);
  let status = if is_error { "error" } else { "success" };
  let duration_ms = duration.as_millis() as u64;
  // Claude gate: a top-level structuredContent makes the host drop content[] and forward only the
  // serialized structuredContent, which escapes the body as \n / \". Plain mode omits
  // structuredContent and emits the content[] blocks verbatim; the body stays in content so no
  // data is lost.
  if plain {
    let error_message = if is_error { Value::String(combined_text(&content)) } else { Value::Null };
    let mut fs_meta = Map::new();
    fs_meta.insert("contentTypes".to_string(), json!(["text"]));
    fs_meta.insert("durationMs".to_string(), json!(duration_ms));
    fs_meta.insert("errorMessage".to_string(), error_message);
    fs_meta.insert("hasStructuredContent".to_string(), Value::Bool(false));
    fs_meta.insert("schemaVersion".to_string(), json!(1));
    fs_meta.insert("status".to_string(), json!(status));
    fs_meta.insert("toolName".to_string(), json!(tool_name));
    fs_meta.extend(result.meta.into_iter().map(|(key, value)| (key, sanitize_json(value))));
    let mut out = Map::new();
    out.insert("content".to_string(), Value::Array(content));
    out.insert("_meta".to_string(), json!({ "fsMcpResult": fs_meta }));
    if is_error {
      out.insert("isError".to_string(), Value::Bool(true));
    }
    return Value::Object(out);
  }
  let compact = compact_enabled();
  // The combined text feeds only error messages and the full-mode data.text copy, so the
  // compact success path skips re-joining (and re-allocating) the whole body.
  let text = if is_error || !compact { combined_text(&content) } else { String::new() };
  let structured = result.structured.unwrap_or(Value::Null);
  let has_structured = !structured.is_null();
  let error = if is_error { json!({ "message": text }) } else { Value::Null };
  let error_message = if is_error { Value::String(text.clone()) } else { Value::Null };
  let mut data = json!({
    "content": content,
    "structuredContent": structured
  });
  if !compact {
    data["text"] = Value::String(text.clone());
  }
  // Compact keeps {data, durationMs} (+error only on failure); error:null, schemaVersion,
  // status, and toolName re-state what the caller and isError already convey on every call.
  let standard = if compact {
    let mut map = Map::new();
    map.insert("data".to_string(), data);
    map.insert("durationMs".to_string(), json!(duration_ms));
    if is_error {
      map.insert("error".to_string(), error);
    }
    Value::Object(map)
  }
  else {
    json!({
      "data": data,
      "durationMs": duration_ms,
      "error": error,
      "schemaVersion": 1,
      "status": status,
      "toolName": tool_name
    })
  };
  let display = create_display_text(tool_name, status, &standard, duration_ms);
  let mut fs_meta = Map::new();
  fs_meta.insert("contentTypes".to_string(), json!(["text"]));
  fs_meta.insert("durationMs".to_string(), json!(duration_ms));
  fs_meta.insert("errorMessage".to_string(), error_message);
  fs_meta.insert("hasStructuredContent".to_string(), Value::Bool(has_structured));
  fs_meta.insert("schemaVersion".to_string(), json!(1));
  fs_meta.insert("status".to_string(), json!(status));
  fs_meta.insert("toolName".to_string(), json!(tool_name));
  fs_meta.extend(result.meta.into_iter().map(|(key, value)| (key, sanitize_json(value))));
  let meta = json!({ "fsMcpResult": fs_meta });
  let mut out = Map::new();
  out.insert("content".to_string(), json!([text_content(display)]));
  out.insert("structuredContent".to_string(), standard);
  out.insert("_meta".to_string(), meta);
  if is_error {
    out.insert("isError".to_string(), Value::Bool(true));
  }
  Value::Object(out)
}
// 6. Normalize content -----------------------------------------------------------------------
fn normalize_content(content: Vec<Value>) -> Vec<Value> {
  if content.is_empty() {
    return vec![text_content(String::new())];
  }
  content
    .into_iter()
    .map(|item| {
      if item.get("type").and_then(Value::as_str) == Some("text") {
        let text = item.get("text").and_then(Value::as_str).unwrap_or("");
        text_content(text.to_string())
      }
      else {
      	sanitize_json(item)
      }
    })
    .collect()
}
// 7. Combined text --------------------------------------------------------------------------
// Accumulate directly into a pre-sized String without an intermediate `Vec<&str>` allocation.
fn combined_text(content: &[Value]) -> String {
  let mut total_len = 0usize;
  let mut first = true;
  for item in content {
    if let Some(text) = item.get("text").and_then(Value::as_str) {
      if !first {
        total_len += 1;
      }
      total_len += text.len();
      first = false;
    }
  }
  let mut out = String::with_capacity(total_len);
  let mut first = true;
  for item in content {
    if let Some(text) = item.get("text").and_then(Value::as_str) {
      if !first {
        out.push('\n');
      }
      out.push_str(text);
      first = false;
    }
  }
  out
}
// 8. Display text --------------------------------------------------------------------------
fn create_display_text(tool_name: &str, status: &str, standard: &Value, duration_ms: u64) -> String {
  let items = standard["data"]["structuredContent"]["totalCount"].as_u64().or_else(|| standard["data"]["structuredContent"]["results"].as_array().map(|items| items.len() as u64)).unwrap_or(1);
  let duration = duration_ms as f64 / 1000.0;

  format!(
    "\u{1b}[38;5;214m-----------------------------------\u{1b}[0m\n\
          \u{1b}[38;5;231m• tool = \u{1b}[38;2;0;180;216m{tool_name}\u{1b}[0m\n\
          \u{1b}[38;5;231m• items = \u{1b}[38;2;0;180;216m{items}\u{1b}[0m\n\
          \u{1b}[38;5;231m• status = \u{1b}[38;2;0;180;216m{status}\u{1b}[0m\n\
          \u{1b}[38;5;231m• duration = \u{1b}[38;2;0;180;216m{duration:.3} sec\u{1b}[0m\n\
          \u{1b}[38;5;214m-----------------------------------\u{1b}[0m"
  )
}
// 9. Sanitize text -------------------------------------------------------------------------
pub fn sanitize_text(value: &str) -> String {
  if value.contains(END_TOKEN) {
    value.replace(END_TOKEN, SAFE_END_TOKEN)
  }
  else {
  	value.to_string()
  }
}
// When the token is absent move the owned String through unchanged to avoid a reallocation.
fn sanitize_owned(text: String) -> String {
  if text.contains(END_TOKEN) {
    text.replace(END_TOKEN, SAFE_END_TOKEN)
  }
  else {
  	text
  }
}
// 10. Sanitize JSON ------------------------------------------------------------------------
pub fn sanitize_json(value: Value) -> Value {
  match value {
    Value::String(text) => Value::String(sanitize_owned(text)),
    Value::Array(items) => Value::Array(items.into_iter().map(sanitize_json).collect()),
    Value::Object(map) => Value::Object(map.into_iter().map(|(key, value)| (key, sanitize_json(value))).collect()),
    item => item,
  }
}
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn normalizes_error_result() {
    let result = build_envelope("x", RawResult::error("boom"), Duration::from_millis(1), false);
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["error"]["message"], "Error: boom");
    assert_eq!(result["_meta"]["fsMcpResult"]["status"], "error");
  }
  #[test]
  fn compact_success_envelope_drops_static_fields() {
    if !compact_enabled() {
      return;
    }
    let raw = RawResult::structured("body", json!({ "totalCount": 1 }));
    let result = build_envelope("x", raw, Duration::from_millis(1), false);
    let standard = &result["structuredContent"];
    assert!(standard.get("error").is_none());
    assert!(standard.get("schemaVersion").is_none());
    assert!(standard.get("status").is_none());
    assert!(standard.get("toolName").is_none());
    assert!(standard["durationMs"].is_u64());
    assert_eq!(standard["data"]["content"][0]["text"], "body");
  }
  #[test]
  fn plain_mode_omits_structured_and_keeps_plain_body() {
    let raw = RawResult::structured("line1\nline2", json!({ "totalCount": 1 }));
    let result = build_envelope("file-read", raw, Duration::from_millis(1), true);
    assert!(result.get("structuredContent").is_none());
    assert_eq!(result["content"][0]["type"], "text");
    assert_eq!(result["content"][0]["text"], "line1\nline2");
    assert_eq!(result["_meta"]["fsMcpResult"]["hasStructuredContent"], false);
  }
}
