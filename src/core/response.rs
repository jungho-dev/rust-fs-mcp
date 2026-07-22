//! response.rs
//! core::response
//!
//! Normalizes a RawResult into the public MCP envelope (content / structuredContent / _meta / isError).
//! Enforces the fixed output-size budget, then applies text and JSON sanitization plus duration
//! measurement on the same path.
//!

use serde_json::{json, Map, Value};
use std::time::Duration;

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
pub fn compact_enabled() -> bool {
  true
}
pub fn normalize_tool_result(tool_name: &str, mut result: RawResult, duration: Duration) -> Value {
  enforce_output_budget(&mut result, MAX_STANDARD_BYTES);
  build_envelope(tool_name, result, duration)
}
fn build_envelope(tool_name: &str, result: RawResult, duration: Duration) -> Value {
  let is_error = result.is_error;
  let content = normalize_content(result.content);
  let status = if is_error { "error" } else { "success" };
  let duration_ms = duration.as_millis() as u64;
  let compact = true;
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
  let output_truncated = result.meta.get("outputTruncated").and_then(Value::as_bool).unwrap_or(false);
  let display = create_display_text(tool_name, status, &standard, duration_ms, output_truncated);
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
// 5a. Output budget ---------------------------------------------------------------------------
// MCP clients hard-reject oversized results instead of truncating them: Claude Code caps a
// tool result at MAX_MCP_OUTPUT_TOKENS (default 25,000 tokens at a ~4 chars/token estimate,
// so ~100,000 serialized chars of the structuredContent payload) and the whole call is lost.
// Budget below that ceiling and degrade to a truncated body the model can page through.
// Fixed constants on purpose: runtime behavior is not configurable (readme Fixed Behavior).
const MAX_STANDARD_BYTES: usize = 88_000;
// Extra raw bytes cut past the measured overflow: reserves room for the truncation notice
// and guarantees every pass strictly shrinks the serialized payload.
const TRUNCATION_SLACK_BYTES: usize = 256;
// Serialized-size allowance for the fixed envelope keys wrapping content/structuredContent.
const ENVELOPE_OVERHEAD_BYTES: usize = 512;

fn enforce_output_budget(result: &mut RawResult, budget: usize) {
  // On failure the combined text is duplicated into error.message and the _meta errorMessage
  // mirror, so each retained text byte serializes three times.
  let copies = if result.is_error { 3 } else { 1 };
  let mut truncated = false;
  for _ in 0..8 {
    // serde_json renders a missing structured payload as `null` (4 bytes).
    let structured_bytes = result.structured.as_ref().map(serialized_len).unwrap_or(4);
    let content_bytes: usize = result.content.iter().map(content_item_bytes).sum();
    let total = content_bytes * copies + structured_bytes + ENVELOPE_OVERHEAD_BYTES;
    if total <= budget {
      break;
    }
    let overflow = total - budget;
    if truncate_largest_text(&mut result.content, overflow.div_ceil(copies) + TRUNCATION_SLACK_BYTES) {
      truncated = true;
      continue;
    }
    if drop_structured_tail(result.structured.as_mut(), overflow) {
      truncated = true;
      continue;
    }
    if stub_structured(&mut result.structured) {
      truncated = true;
      continue;
    }
    break;
  }
  if truncated {
    result.meta.insert("outputTruncated".to_string(), Value::Bool(true));
  }
}
fn serialized_len(value: &Value) -> usize {
  serde_json::to_string(value).map(|text| text.len()).unwrap_or(0)
}
// Non-text blocks (images) are exempt: clients meter them separately and a cut base64 body
// would be corrupt rather than shorter, so they only count a fixed wrapper allowance.
fn content_item_bytes(item: &Value) -> usize {
  if item.get("type").and_then(Value::as_str) == Some("text") {
    serialized_len(item) + 1
  }
  else {
    64
  }
}
fn truncate_largest_text(content: &mut [Value], cut_bytes: usize) -> bool {
  let mut largest: Option<(usize, usize)> = None;
  for (index, item) in content.iter().enumerate() {
    if item.get("type").and_then(Value::as_str) != Some("text") {
      continue;
    }
    let length = item.get("text").and_then(Value::as_str).map(str::len).unwrap_or(0);
    // Bodies at or below the notice budget are not worth cutting; this also keeps an
    // already-appended notice from being re-truncated on later passes.
    if length > TRUNCATION_SLACK_BYTES && largest.is_none_or(|(_, best)| length > best) {
      largest = Some((index, length));
    }
  }
  let Some((index, length)) = largest else {
    return false;
  };
  let Some(text) = content[index].get("text").and_then(Value::as_str) else {
    return false;
  };
  let keep = floor_char_boundary(text, length.saturating_sub(cut_bytes));
  let notice = format!("\n\n[truncated: kept {keep} of {length} body bytes to fit the client output limit; request smaller slices (offset/length, start_line/line_count, maxResults, maxEntries) or fewer batch items]");
  let mut body = String::with_capacity(keep + notice.len());
  body.push_str(&text[..keep]);
  body.push_str(&notice);
  content[index]["text"] = Value::String(body);
  true
}
// Batch structured entries drop from the tail once the text body alone cannot shrink the
// payload enough; resultsDropped records how many entries were shed.
fn drop_structured_tail(structured: Option<&mut Value>, overflow: usize) -> bool {
  let Some(root) = structured else {
    return false;
  };
  let Some(results) = root.get_mut("results").and_then(Value::as_array_mut) else {
    return false;
  };
  if results.len() <= 1 {
    return false;
  }
  let mut shed = 0usize;
  let mut dropped = 0u64;
  while results.len() > 1 && shed < overflow + TRUNCATION_SLACK_BYTES {
    let Some(entry) = results.pop() else {
      break;
    };
    shed += serialized_len(&entry) + 1;
    dropped += 1;
  }
  if dropped == 0 {
    return false;
  }
  let already = root.get("resultsDropped").and_then(Value::as_u64).unwrap_or(0);
  root["resultsDropped"] = json!(already + dropped);
  true
}
// Last resort for a single oversized structured payload without a results[] tail: replace it
// with a stub so the response never breaches the client ceiling.
fn stub_structured(structured: &mut Option<Value>) -> bool {
  let Some(root) = structured else {
    return false;
  };
  if root.get("structuredTruncated").is_some() {
    return false;
  }
  let original = serialized_len(root);
  if original <= ENVELOPE_OVERHEAD_BYTES {
    return false;
  }
  *structured = Some(json!({ "structuredTruncated": true, "originalBytes": original }));
  true
}
// str::floor_char_boundary is still unstable; clamp the cut index down to a char boundary.
fn floor_char_boundary(text: &str, index: usize) -> usize {
  if index >= text.len() {
    return text.len();
  }
  let mut boundary = index;
  while boundary > 0 && !text.is_char_boundary(boundary) {
    boundary -= 1;
  }
  boundary
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
fn create_display_text(tool_name: &str, status: &str, standard: &Value, duration_ms: u64, truncated: bool) -> String {
  let items = standard["data"]["structuredContent"]["totalCount"].as_u64().or_else(|| standard["data"]["structuredContent"]["results"].as_array().map(|items| items.len() as u64)).unwrap_or(1);
  let duration = duration_ms as f64 / 1000.0;
  let truncated_line = if truncated {
    "\u{1b}[38;5;231m• truncated = \u{1b}[38;2;0;180;216mtrue\u{1b}[0m\n"
  }
  else {
    ""
  };

  format!(
    "\u{1b}[38;5;214m-----------------------------------\u{1b}[0m\n\
          \u{1b}[38;5;231m• tool = \u{1b}[38;2;0;180;216m{tool_name}\u{1b}[0m\n\
          \u{1b}[38;5;231m• items = \u{1b}[38;2;0;180;216m{items}\u{1b}[0m\n\
          \u{1b}[38;5;231m• status = \u{1b}[38;2;0;180;216m{status}\u{1b}[0m\n\
          {truncated_line}\
          \u{1b}[38;5;231m• duration = \u{1b}[38;2;0;180;216m{duration:.3} sec\u{1b}[0m\n\
          \u{1b}[38;5;214m-----------------------------------\u{1b}[0m"
  )
}
// 9. Sanitize text -------------------------------------------------------------------------
// go-fs-mcp parity: the end-token sanitizer is neutered upstream (end and safe tokens are
// identical), so both sanitizers pass values through. This removes the per-response contains
// scan over large bodies and the full recursive rebuild of every structured Value — one of
// the largest per-call allocation sources. The functions stay as the single seam to
// re-enable sanitization later.
pub fn sanitize_text(value: &str) -> String {
  value.to_string()
}
fn sanitize_owned(text: String) -> String {
  text
}
// 10. Sanitize JSON ------------------------------------------------------------------------
pub fn sanitize_json(value: Value) -> Value {
  value
}
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn normalizes_error_result() {
    let result = build_envelope("x", RawResult::error("boom"), Duration::from_millis(1));
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["error"]["message"], "Error: boom");
    assert_eq!(result["_meta"]["fsMcpResult"]["status"], "error");
  }
  #[test]
  fn compact_success_envelope_drops_static_fields() {
    let raw = RawResult::structured("body", json!({ "totalCount": 1 }));
    let result = build_envelope("x", raw, Duration::from_millis(1));
    let standard = &result["structuredContent"];
    assert!(standard.get("error").is_none());
    assert!(standard.get("schemaVersion").is_none());
    assert!(standard.get("status").is_none());
    assert!(standard.get("toolName").is_none());
    assert!(standard["durationMs"].is_u64());
    assert_eq!(standard["data"]["content"][0]["text"], "body");
  }
  #[test]
  fn output_budget_truncates_oversized_text() {
    let mut raw = RawResult::structured("x".repeat(50_000), json!({ "totalCount": 1 }));
    enforce_output_budget(&mut raw, 10_000);
    let text = raw.content[0]["text"].as_str().unwrap();
    assert!(text.len() < 10_000, "{}", text.len());
    assert!(text.contains("[truncated: kept"));
    assert_eq!(raw.meta["outputTruncated"], Value::Bool(true));
  }
  #[test]
  fn output_budget_keeps_small_results_byte_identical() {
    let mut raw = RawResult::structured("small body", json!({ "totalCount": 1 }));
    enforce_output_budget(&mut raw, MAX_STANDARD_BYTES);
    assert_eq!(raw.content[0]["text"], "small body");
    assert!(!raw.meta.contains_key("outputTruncated"));
  }
  #[test]
  fn output_budget_counts_error_text_copies() {
    let mut raw = RawResult::error("e".repeat(30_000));
    enforce_output_budget(&mut raw, 10_000);
    let text = raw.content[0]["text"].as_str().unwrap().to_string();
    assert!(text.contains("[truncated: kept"));
    let envelope = build_envelope("x", raw, Duration::from_millis(1));
    let standard = serde_json::to_string(&envelope["structuredContent"]).unwrap();
    assert!(standard.len() <= 10_000, "{}", standard.len());
  }
  #[test]
  fn output_budget_drops_structured_tail_when_text_is_small() {
    let results: Vec<Value> = (0..500)
      .map(|index| json!({ "index": index, "ok": true, "data": { "path": format!("C:/tmp/file-{index}.txt") } }))
      .collect();
    let mut raw = RawResult::structured("list", json!({ "results": results, "totalCount": 500 }));
    enforce_output_budget(&mut raw, 8_000);
    let structured = raw.structured.as_ref().unwrap();
    let kept = structured["results"].as_array().unwrap().len() as u64;
    let dropped = structured["resultsDropped"].as_u64().unwrap();
    assert!(kept < 500);
    assert_eq!(kept + dropped, 500);
  }
  #[test]
  fn output_budget_exempts_image_blocks() {
    let mut raw = RawResult {
      content: vec![json!({ "type": "image", "data": "A".repeat(200_000), "mimeType": "image/png" })],
      structured: Some(json!({ "bytes": 200_000 })),
      is_error: false,
      meta: Map::new(),
    };
    enforce_output_budget(&mut raw, 10_000);
    assert_eq!(raw.content[0]["data"].as_str().unwrap().len(), 200_000);
    assert!(!raw.meta.contains_key("outputTruncated"));
  }
  #[test]
  fn normalized_oversized_result_fits_client_ceiling() {
    let raw = RawResult::structured("y".repeat(400_000), json!({ "totalCount": 1 }));
    let result = normalize_tool_result("file-read", raw, Duration::from_millis(1));
    let standard = serde_json::to_string(&result["structuredContent"]).unwrap();
    assert!(standard.len() <= MAX_STANDARD_BYTES, "{}", standard.len());
    assert_eq!(result["_meta"]["fsMcpResult"]["outputTruncated"], Value::Bool(true));
  }
}
