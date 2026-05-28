use crate::protocol::catalog::tool_catalog;
use crate::tools::dispatch_tool_call;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

const SERVER_INSTRUCTIONS: &str = "Use rust-fs-mcp for local filesystem, search, and git work.\nBatch-first rule: when one task needs multiple file, directory, search, or git operations of the same kind, put every item into one rust-fs-mcp tool call instead of calling the same tool repeatedly.";

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
    json!({
        "capabilities": {
            "logging": {},
            "resources": {},
            "tools": {}
        },
        "instructions": SERVER_INSTRUCTIONS,
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
}
