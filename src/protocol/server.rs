//! server.rs
//! protocol::server
//!
//! JSON-RPC processing loop over stdin / stdout one line at a time.
//! Routes the initialize, tools/list, tools/call, resources/list, and resources/templates/list methods.
//! tools/list responses splice the pre-serialized catalog bytes straight into the envelope.
//!

use crate::protocol::catalog::wire_tools_json;
use crate::tools::dispatch_tool_call;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

const SERVER_INSTRUCTIONS: &str = "Use rust-fs-mcp for local filesystem, search, and git work.\nBatch-first rule: when one task needs multiple file, directory, search, or git operations of the same kind, put every item into one rust-fs-mcp tool call instead of calling the same tool repeatedly.";
// MCP 버전 협상: 아는 버전이면 에코, 모르는 버전이면 서버가 지원하는 최신 버전을 제안한다.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

// 응답 분류: tools/list는 사전 직렬화된 카탈로그 바이트를 그대로 쓰고(재직렬화 0),
// 나머지는 기존처럼 Value 응답을 직렬화한다.
enum LineReply {
  Full(Value),
  CachedToolsList(Value),
}
// 1. Run server ----------------------------------------------------------------------------
// tools/call은 요청별 워커 스레드로 디스패치해 느린 도구(web-render, 대형 검색, git)가
// 다른 요청을 막지 않는다. 응답 쓰기는 공유 뮤텍스로 직렬화하고(JSON-RPC는 id 매칭이라
// 순서 무관), 가베운 메서드(initialize, tools/list 등)는 기존처럼 인라인 즉답한다.
type SharedWriter = Arc<Mutex<io::BufWriter<io::Stdout>>>;
const MAX_INFLIGHT_CALLS: usize = 64;
// 상주 디스패치 워커 수: 요청당 thread::spawn(~130µs 실측)을 짧은 호출에서 제거한다.
// 워커가 전부 장기 작업(web-render 등)에 점유되면 spawn으로 폴백해 동시성을 유지한다.
const DISPATCH_WORKERS: usize = 4;

struct InFlight {
  count: Mutex<usize>,
  done: Condvar,
}
pub fn run() -> Result<(), String> {
  let stdin = io::stdin();
  let writer: SharedWriter = Arc::new(Mutex::new(io::BufWriter::with_capacity(64 * 1024, io::stdout())));
  let mut reader = stdin.lock();
  // 요청 줄 버퍼는 프로세스 수명 동안 재사용(줄마다 String 할당 제거, 용량 유지).
  let mut line = String::new();
  let in_flight = Arc::new(InFlight { count: Mutex::new(0), done: Condvar::new() });
  // 상주 워커는 단일 Receiver를 뮤텍스로 나눠 잡고, idle 카운터로 "지금 받을 수 있는
  // 워커가 있는지"를 디스패처에 알린다. 채널은 run 종료 시 drop되어 워커를 회수한다.
  let (call_tx, call_rx) = mpsc::channel::<Value>();
  let call_rx = Arc::new(Mutex::new(call_rx));
  let idle_workers = Arc::new(AtomicUsize::new(0));
  for _ in 0..DISPATCH_WORKERS {
    let worker_rx = Arc::clone(&call_rx);
    let worker_writer = Arc::clone(&writer);
    let worker_flight = Arc::clone(&in_flight);
    let worker_idle = Arc::clone(&idle_workers);
    let _ = thread::Builder::new().name("fs-mcp-call".to_string()).spawn(move || {
      loop {
        worker_idle.fetch_add(1, Ordering::Release);
        let received = worker_rx.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).recv();
        worker_idle.fetch_sub(1, Ordering::Release);
        let Ok(request) = received else {
          break;
        };
        let reply = handle_request_reply(&request);
        let _ = write_reply(&worker_writer, reply);
        finish_in_flight(&worker_flight);
      }
    });
  }
  loop {
    line.clear();
    match reader.read_line(&mut line) {
      Ok(0) => break,
      Ok(_) => {}
      // 비 UTF-8 입력 한 줄은 서버를 죽이지 않고 파스 에러로 응답 후 계속 처리.
      Err(error) if error.kind() == io::ErrorKind::InvalidData => {
        let response = error_response(Value::Null, -32700, &format!("Parse error: {error}"));
        write_reply(&writer, LineReply::Full(response))?;
        continue;
      }
      Err(_) => break,
    }
    if line.trim().is_empty() {
      continue;
    }
    match classify_line(&line) {
      Inbound::Ignore => {}
      Inbound::Reply(reply) => write_reply(&writer, reply)?,
      Inbound::ToolCall(request) => dispatch_tool_request(request, &writer, &in_flight, &call_tx, &idle_workers),
    }
  }
  // EOF 후에도 진행 중인 tools/call 응답이 모두 나갈 때까지 기다린다.
  let mut count = in_flight.count.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  while *count > 0 {
    count = in_flight.done.wait(count).unwrap_or_else(|poisoned| poisoned.into_inner());
  }
  Ok(())
}
// 1-0. 인바운드 분류: tools/call만 워커로, 나머지는 인라인 즉답 ------------------------------
enum Inbound {
  Ignore,
  Reply(LineReply),
  ToolCall(Value),
}
fn classify_line(line: &str) -> Inbound {
  let request: Value = match serde_json::from_str(line) {
    Ok(value) => value,
    Err(error) => {
      return Inbound::Reply(LineReply::Full(error_response(Value::Null, -32700, &format!("Parse error: {error}"))));
    }
  };
  // 알림(notification)은 id 없는 request. 응답 금지(JSON-RPC 2.0). id 있는 request는 반드시 응답.
  if request.get("id").is_none() {
    return Inbound::Ignore;
  }
  if request.get("method").and_then(Value::as_str) == Some("tools/call") {
    return Inbound::ToolCall(request);
  }
  Inbound::Reply(handle_request_reply(&request))
}
fn dispatch_tool_request(request: Value, writer: &SharedWriter, in_flight: &Arc<InFlight>, call_tx: &mpsc::Sender<Value>, idle_workers: &Arc<AtomicUsize>) {
  // 폭주 방어: 동시 tools/call이 상한이면 인라인 처리로 자연 백프레셔.
  {
    let mut count = in_flight.count.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *count >= MAX_INFLIGHT_CALLS {
      drop(count);
      let reply = handle_request_reply(&request);
      let _ = write_reply(writer, reply);
      return;
    }
    *count += 1;
  }
  // 유휴 상주 워커가 있으면 채널로(스폰 없음), 전부 바쁘면 spawn으로 동시성 유지.
  let request = if idle_workers.load(Ordering::Acquire) > 0 {
    match call_tx.send(request) {
      Ok(()) => return,
      Err(mpsc::SendError(request)) => request,
    }
  }
  else {
    request
  };
  let id = request.get("id").cloned().unwrap_or(Value::Null);
  let job_writer = Arc::clone(writer);
  let job_flight = Arc::clone(in_flight);
  let spawned = thread::Builder::new().name("fs-mcp-call".to_string()).spawn(move || {
    let reply = handle_request_reply(&request);
    // 워커의 쓰기 실패(stdout 닫힘)는 종료 국면이므로 무시한다.
    let _ = write_reply(&job_writer, reply);
    finish_in_flight(&job_flight);
  });
  if spawned.is_err() {
    // 스레드 생성 실패(극한 자원 고갈) 시 카운터 원복 후 에러 응답으로 행 방지.
    finish_in_flight(in_flight);
    let response = error_response(id, -32603, "Failed to spawn tool worker");
    let _ = write_reply(writer, LineReply::Full(response));
  }
}
fn finish_in_flight(in_flight: &InFlight) {
  let mut count = in_flight.count.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  *count -= 1;
  if *count == 0 {
    in_flight.done.notify_all();
  }
}
// 1-1. 응답 1건 직렬화·flush: 직렬화(100KB당 ~70µs 실측)는 락 밖 버퍼에서 끝내고,
// 공유 라이터 잠금은 완성된 바이트의 write_all+flush 동안만 잡는다(병렬 응답 경합 제거).
fn write_reply(writer: &SharedWriter, reply: LineReply) -> Result<(), String> {
  let mut buffer = Vec::with_capacity(4 * 1024);
  match reply {
    LineReply::CachedToolsList(id) => encode_tools_list(&mut buffer, &id)?,
    LineReply::Full(response) => encode_response(&mut buffer, &response)?,
  }
  let mut guard = writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  guard.write_all(&buffer).map_err(|error| error.to_string())?;
  guard.flush().map_err(|error| error.to_string())
}
fn encode_response(buffer: &mut Vec<u8>, response: &Value) -> Result<(), String> {
  serde_json::to_writer(&mut *buffer, response).map_err(|error| error.to_string())?;
  buffer.push(b'\n');
  Ok(())
}
// 1-2. tools/list 원시 카탈로그 스플라이스 ---------------------------------------------------
fn encode_tools_list(buffer: &mut Vec<u8>, id: &Value) -> Result<(), String> {
  buffer.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":");
  serde_json::to_writer(&mut *buffer, id).map_err(|error| error.to_string())?;
  buffer.extend_from_slice(b",\"result\":");
  buffer.extend_from_slice(wire_tools_json().as_bytes());
  buffer.extend_from_slice(b"}\n");
  Ok(())
}
// 2. Handle protocol line ------------------------------------------------------------------
// 동기 경로(테스트·임베더)와 워커 경로가 같은 classify/handle을 공유한다.
fn handle_line_reply(line: &str) -> Option<LineReply> {
  match classify_line(line) {
    Inbound::Ignore => None,
    Inbound::Reply(reply) => Some(reply),
    Inbound::ToolCall(request) => Some(handle_request_reply(&request)),
  }
}
fn handle_request_reply(request: &Value) -> LineReply {
  let method = request.get("method").and_then(Value::as_str).unwrap_or("");
  let id = request.get("id").cloned().unwrap_or(Value::Null);
  match method {
    "initialize" => LineReply::Full(success_response(id, initialize_result(request))),
    "tools/list" => LineReply::CachedToolsList(id),
    "tools/call" => LineReply::Full(success_response(id, call_tool_result(request))),
    "resources/list" => LineReply::Full(success_response(id, json!({ "resources": [] }))),
    "resources/templates/list" => LineReply::Full(success_response(id, json!({ "resourceTemplates": [] }))),
    _ => LineReply::Full(error_response(id, -32601, &format!("Method not found: {method}"))),
  }
}
// 테스트·임베더 호환 경로: 캐시된 tools/list 바이트를 Value로 재구성해 완성 응답을 돌려준다.
pub fn handle_line(line: &str) -> Option<Value> {
  match handle_line_reply(line)? {
    LineReply::Full(response) => Some(response),
    LineReply::CachedToolsList(id) => {
      let tools: Value = serde_json::from_str(wire_tools_json()).unwrap_or_else(|_| json!({ "tools": [] }));
      Some(success_response(id, tools))
    }
  }
}
// 3. Initialize result --------------------------------------------------------------------
fn initialize_result(request: &Value) -> Value {
  let protocol_version = match request["params"]["protocolVersion"].as_str() {
    Some(version) if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) => version,
    _ => LATEST_PROTOCOL_VERSION,
  };
  json!({
    "capabilities": {
      "logging": {},
      "resources": {},
      "tools": {}
    },
    "instructions": SERVER_INSTRUCTIONS,
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
    assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 23);
  }
  #[test]
  fn cached_tools_list_wire_matches_catalog() {
    // 스플라이스되는 원시 바이트가 카탈로그와 구조적으로 동일해야 한다.
    let wire: Value = serde_json::from_str(wire_tools_json()).unwrap();
    assert_eq!(wire["tools"], Value::Array(crate::protocol::catalog::tool_catalog()));
  }
  #[test]
  fn uses_fixed_instructions_for_every_client() {
    let claude = handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"claude-code","version":"2.1.0"}}}"#).unwrap();
    let claude_text = claude["result"]["instructions"].as_str().unwrap();
    assert_eq!(claude_text, SERVER_INSTRUCTIONS);
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
  #[test]
  fn negotiates_protocol_version() {
    // 지원 버전은 에코, 모르는 미래 버전은 서버가 지원하는 최신 버전으로 응답한다.
    let known = handle_line(r#"{"jsonrpc":"2.0","id":8,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#).unwrap();
    assert_eq!(known["result"]["protocolVersion"], "2025-03-26");
    let unknown = handle_line(r#"{"jsonrpc":"2.0","id":9,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#).unwrap();
    assert_eq!(unknown["result"]["protocolVersion"], LATEST_PROTOCOL_VERSION);
  }
}
