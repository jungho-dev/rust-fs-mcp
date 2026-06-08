//! server.rs
//! protocol::server
//!
//! JSON-RPC processing loop over stdin / stdout one line at a time.
//! Routes the initialize, tools/list, tools/call, resources/list, and resources/templates/list methods.
//!

use crate::protocol::catalog::tool_catalog;
use crate::tools::dispatch_tool_call;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

const SERVER_INSTRUCTIONS: &str = "Use rust-fs-mcp for local filesystem, search, and git work.\nBatch-first rule: when one task needs multiple file, directory, search, or git operations of the same kind, put every item into one rust-fs-mcp tool call instead of calling the same tool repeatedly.";
const CLAUDE_GATE_INSTRUCTIONS: &str = "rust-fs-mcp supplements the built-in tools; it does not replace them.\nFor a single file read, a single content search, or a one-off git lookup, prefer the built-in tools.\nCall rust-fs-mcp when one call replaces several built-in calls: 2+ same-kind operations batched into one items[]/paths[] call, probing many possibly-missing paths with allowMissing, line-number edits via file-edit-lines, paginated search sessions over huge result sets, and *_path/args_path indirection for large arguments.\nNever split same-kind multi-item work into repeated single-item calls.";

// 1. Run server ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn run() -> Result<(), String> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut writer = io::BufWriter::new(stdout.lock());
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| error.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&line) {
            serde_json::to_writer(&mut writer, &response).map_err(|error| error.to_string())?;
            writer.write_all(b"\n").map_err(|error| error.to_string())?;
            writer.flush().map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

// 2. Handle protocol line ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn handle_line(line: &str) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            return Some(error_response(
                Value::Null,
                -32700,
                &format!("Parse error: {error}"),
            ));
        }
    };
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    if method.starts_with("notifications/") {
        return None;
    }
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => Some(success_response(id, initialize_result(&request))),
        "tools/list" => Some(success_response(id, json!({ "tools": tool_catalog() }))),
        "tools/call" => Some(success_response(id, call_tool_result(&request))),
        "resources/list" => Some(success_response(id, json!({ "resources": [] }))),
        "resources/templates/list" => {
            Some(success_response(id, json!({ "resourceTemplates": [] })))
        }
        _ => Some(error_response(
            id,
            -32601,
            &format!("Method not found: {method}"),
        )),
    }
}

// 3. Initialize result ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
fn initialize_result(request: &Value) -> Value {
    let protocol_version = request["params"]["protocolVersion"]
        .as_str()
        .unwrap_or("2025-06-18");
    let client_name = request["params"]["clientInfo"]["name"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_claude = client_name.contains("claude");
    crate::core::response::set_plain_content_mode(is_claude);
    let instructions = if is_claude {
        CLAUDE_GATE_INSTRUCTIONS
    } else {
        SERVER_INSTRUCTIONS
    };
    json!({
        "capabilities": {
            "logging": {},
            "resources": {},
            "tools": {}
        },
        "instructions": instructions,
        "protocolVersion": protocol_version,
        "serverInfo": {
            "name": "rust-fs-mcp",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

// 4. Call tool result ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
fn call_tool_result(request: &Value) -> Value {
    let params = &request["params"];
    let name = params["name"].as_str().unwrap_or("");
    let args = params.get("arguments").cloned();
    dispatch_tool_call(name, args)
}

// 5. Success response ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

// 6. Error response ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
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
        assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 22);
    }

    #[test]
    fn branches_instructions_by_client() {
        let claude = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"claude-code","version":"2.1.0"}}}"#,
        )
        .unwrap();
        let claude_text = claude["result"]["instructions"].as_str().unwrap();
        assert_eq!(claude_text, CLAUDE_GATE_INSTRUCTIONS);
        let codex = handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"codex","version":"0.1.0"}}}"#,
        )
        .unwrap();
        let codex_text = codex["result"]["instructions"].as_str().unwrap();
        assert_eq!(codex_text, SERVER_INSTRUCTIONS);
        let bare = handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"initialize"}"#).unwrap();
        assert_eq!(
            bare["result"]["instructions"].as_str().unwrap(),
            SERVER_INSTRUCTIONS
        );
    }
}
