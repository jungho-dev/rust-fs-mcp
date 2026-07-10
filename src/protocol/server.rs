//! server.rs
//! protocol::server
//!
//! JSON-RPC processing loop over stdin / stdout one line at a time.
//! Routes the initialize, tools/list, tools/call, resources/list, and resources/templates/list methods.
//!

use crate::protocol::catalog::tool_catalog;
use crate::tools::dispatch_tool_call;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

const SERVER_INSTRUCTIONS: &str = "Use rust-fs-mcp for local filesystem, search, and git work.\nBatch-first rule: when one task needs multiple file, directory, search, or git operations of the same kind, put every item into one rust-fs-mcp tool call instead of calling the same tool repeatedly.";
const CLAUDE_GATE_INSTRUCTIONS: &str = "rust-fs-mcp supplements the built-in tools; it does not replace them.\nFor a single file read or a single content search, prefer the built-in tools.\nCall rust-fs-mcp when one call replaces several built-in calls: 2+ same-kind operations batched into one items[]/paths[] call, probing many possibly-missing paths with allowMissing, line-number edits via file-edit-lines, and *_path/args_path indirection for large arguments.\nFor git status/diff, directory listing or probing, and multi-file existence probes, use rust-fs-mcp even for a single operation — the built-in path shells out and is slower.\nNever split same-kind multi-item work into repeated single-item calls.";

// 1. Run server ----------------------------------------------------------------------------
pub fn run() -> Result<(), String> {
  let stdin = io::stdin();
  let stdout = io::stdout();
  let mut writer = io::BufWriter::new(stdout.lock());
  for line in stdin.lock().lines() {
    let line = match line {
      Ok(line) => line,
      // 비 UTF-8 입력 한 줄은 서버를 죽이지 않고 파스 에러로 응답 후 계속 처리.
      Err(error) if error.kind() == io::ErrorKind::InvalidData => {
        let response = error_response(Value::Null, -32700, &format!("Parse error: {error}"));
        write_response(&mut writer, &response)?;
        continue;
      }
      Err(_) => break,
    };
    if line.trim().is_empty() {
      continue;
    }
    if let Some(response) = handle_line(&line) {
      write_response(&mut writer, &response)?;
    }
  }
  Ok(())
}
// 1-1. 응답 1건 직렬화·flush -----------------------------------------------------------------
fn write_response(writer: &mut impl Write, response: &Value) -> Result<(), String> {
  serde_json::to_writer(&mut *writer, response).map_err(|error| error.to_string())?;
  writer.write_all(b"\n").map_err(|error| error.to_string())?;
  writer.flush().map_err(|error| error.to_string())
}
// 2. Handle protocol line ------------------------------------------------------------------
pub fn handle_line(line: &str) -> Option<Value> {
  let request: Value = match serde_json::from_str(line) {
    Ok(value) => value,
    Err(error) => {
      return Some(error_response(Value::Null, -32700, &format!("Parse error: {error}")));
    }
  };
  // 알림(notification)은 id 없는 request. 응답 금지(JSON-RPC 2.0). id 있는 request는 반드시 응답.
  if request.get("id").is_none() {
    return None;
  }
  let method = request.get("method").and_then(Value::as_str).unwrap_or("");
  let id = request.get("id").cloned().unwrap_or(Value::Null);
  match method {
    "initialize" => Some(success_response(id, initialize_result(&request))),
    "tools/list" => Some(success_response(id, json!({ "tools": tool_catalog() }))),
    "tools/call" => Some(success_response(id, call_tool_result(&request))),
    "resources/list" => Some(success_response(id, json!({ "resources": [] }))),
    "resources/templates/list" => Some(success_response(id, json!({ "resourceTemplates": [] }))),
    _ => Some(error_response(id, -32601, &format!("Method not found: {method}"))),
  }
}
// 3. Initialize result --------------------------------------------------------------------
fn initialize_result(request: &Value) -> Value {
  let protocol_version = request["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18");
  let client_name = request["params"]["clientInfo"]["name"].as_str().unwrap_or("").to_ascii_lowercase();
  let is_claude = client_name.contains("claude");
  crate::core::response::set_plain_content_mode(is_claude);
  let instructions = if is_claude { CLAUDE_GATE_INSTRUCTIONS } else { SERVER_INSTRUCTIONS };
  json!({
    "capabilities": {
      "logging": {},
      "resources": {},
      "tools": {}
    },
    "instructions": instructions,
    "protocolVersion": protocol_version,
    "serverInfo": {
      "name":
        "rust-fs-mcp",
      "version": env!("CARGO_PKG_VERSION")
    }
  })
}
// 4. Call tool result --------------------------------------------------------------------
fn call_tool_result(request: &Value) -> Value {
  let params = &request["params"];
  let name = params["name"].as_str().unwrap_or("");
  let args = params.get("arguments").cloned();
  dispatch_tool_call(name, args)
}
// 5. Success response --------------------------------------------------------------------
fn success_response(id: Value, result: Value) -> Value {
  json!({
    "jsonrpc":
      "2.0",
    "id": id,
    "result": result
  })
}
// 6. Error response ----------------------------------------------------------------------
fn error_response(id: Value, code: i64, message: &str) -> Value {
  json!({
    "jsonrpc":
      "2.0",
    "id": id,
    "error": {
      "code": code,
      "message": message
    }
  })
}
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn lists_tools() {
    let response = handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
    assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 24);
  }
  #[test]
  fn branches_instructions_by_client() {
    let claude = handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"claude-code","version":"2.1.0"}}}"#).unwrap();
    let claude_text = claude["result"]["instructions"].as_str().unwrap();
    assert_eq!(claude_text, CLAUDE_GATE_INSTRUCTIONS);
    let codex = handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"codex","version":"0.1.0"}}}"#).unwrap();
    let codex_text = codex["result"]["instructions"].as_str().unwrap();
    assert_eq!(codex_text, SERVER_INSTRUCTIONS);
    let bare = handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"initialize"}"#).unwrap();
    assert_eq!(bare["result"]["instructions"].as_str().unwrap(), SERVER_INSTRUCTIONS);
  }
  #[test]
  fn notification_without_id_gets_no_response() {
    assert!(handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    assert!(handle_line(r#"{"jsonrpc":"2.0","method":"cancelled"}"#).is_none());
  }
  #[test]
  fn request_with_id_always_responds() {
    let response = handle_line(r#"{"jsonrpc":"2.0","id":7,"method":"notifications/foo"}"#).unwrap();
    assert_eq!(response["id"], 7);
    assert_eq!(response["error"]["code"], -32601);
  }
}
