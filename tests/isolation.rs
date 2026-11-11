//! isolation.rs
//!
//! Integration tests for the kill-able fancy-regex worker over the real stdio server.
//! A runaway look-around must fail its item on time while the same server keeps answering.
//! The idle standby worker must serve the next job with the same result and exit with its server.
//!

use rust_fs_mcp::core::isolate;
use rust_fs_mcp::tools::dispatch_tool_call;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// 폭주 패턴 (룩어라운드 본문 O(n) 재실행)
const RUNAWAY_PATTERN: &str = "(?=.*NOPE)a";
const RUNAWAY_BYTES: usize = 400_000;
// 스팬 기록 약 400 KB (축소 응답 상한 초과)
const MANY_LINES: usize = 50_000;
const SMALL_REPLY_CAP: u64 = 64 * 1024;

// 1. Stdio server -------------------------------------------------------------
struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}
impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rust-fs-mcp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn rust-fs-mcp");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = child.stdout.take().expect("server stdout");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            stdin,
            lines,
            next_id: 1,
        };
        let init = server.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "isolation-test", "version": "1" }
            }),
            Duration::from_secs(10),
        );
        assert!(init.is_some(), "initialize did not answer");
        server.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        server
    }
    fn send(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .expect("write request");
        self.stdin.flush().expect("flush request");
    }
    // 응답 대기 (기한 초과 None)
    fn request(&mut self, method: &str, params: Value, wait: Duration) -> Option<Value> {
        let id = self.send_request(method, params);
        self.wait_response(id, wait)
    }
    fn send_request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }
    fn wait_response(&mut self, id: u64, wait: Duration) -> Option<Value> {
        let deadline = Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(remaining).ok()?;
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["id"] == json!(id) {
                return Some(message);
            }
        }
    }
    fn call(&mut self, tool: &str, arguments: Value, wait: Duration) -> Option<Value> {
        self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
            wait,
        )
    }
    // 같은 서버 응답성 확인
    fn assert_responsive(&mut self) {
        let listed = self.request("tools/list", json!({}), Duration::from_secs(5));
        let tools = listed
            .as_ref()
            .and_then(|message| message["result"]["tools"].as_array())
            .map(Vec::len);
        assert!(
            tools.is_some_and(|count| count > 0),
            "tools/list failed after the call: {listed:?}"
        );
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// 2. Response readers ---------------------------------------------------------
// tools/call result 본문 (dispatch 반환형)
fn local_text(result: &Value) -> String {
    result["structuredContent"]["data"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}
fn status(result: &Value) -> &str {
    result["_meta"]["fsMcpResult"]["status"]
        .as_str()
        .unwrap_or_default()
}
fn batch_text(response: &Value) -> String {
    local_text(&response["result"])
}
fn item_data(response: &Value) -> Value {
    response["result"]["structuredContent"]["data"]["structuredContent"]["results"][0]["data"]
        .clone()
}
fn is_error(response: &Value) -> bool {
    response["result"]["isError"].as_bool() == Some(true)
}

// 3. Fixtures -----------------------------------------------------------------
fn temp_dir(prefix: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{prefix}-{stamp}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
fn write_runaway_file(dir: &Path) -> PathBuf {
    let path = dir.join("runaway.txt");
    let mut body = "a".repeat(RUNAWAY_BYTES);
    body.push('\n');
    std::fs::write(&path, body).unwrap();
    path
}

// 4. Runaway fancy-regex file-replace -----------------------------------------
#[test]
fn runaway_fancy_replace_is_killed_and_server_survives() {
    let dir = temp_dir("rust-fs-mcp-isolation-replace");
    let file = write_runaway_file(&dir);
    let before = std::fs::read(&file).unwrap();
    let mut server = Server::start();
    let started = Instant::now();
    let response = server.call(
        "file-replace",
        json!({ "items": [{
            "file_path": file.display().to_string(),
            "pattern": RUNAWAY_PATTERN,
            "replacement": "b",
            "timeout_ms": 1500
        }] }),
        Duration::from_secs(4),
    );
    let elapsed = started.elapsed();
    let response = response.unwrap_or_else(|| panic!("no file-replace answer within 4 s"));
    assert!(is_error(&response), "{response}");
    let text = batch_text(&response);
    assert!(text.contains("1500 ms timeout_ms budget"), "{text}");
    assert!(text.contains("isolated worker was killed"), "{text}");
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    assert_eq!(std::fs::read(&file).unwrap(), before);
    server.assert_responsive();
    std::fs::remove_dir_all(&dir).unwrap();
}

// 5. Runaway fancy-regex fs-search --------------------------------------------
#[test]
fn runaway_fancy_search_is_killed_and_server_survives() {
    let dir = temp_dir("rust-fs-mcp-isolation-search");
    write_runaway_file(&dir);
    let mut server = Server::start();
    let started = Instant::now();
    let response = server.call(
        "fs-search",
        json!({ "items": [{
            "path": dir.display().to_string(),
            "pattern": RUNAWAY_PATTERN,
            "multiline": true,
            "timeout_ms": 1500
        }] }),
        Duration::from_millis(4500),
    );
    let elapsed = started.elapsed();
    let response = response.unwrap_or_else(|| panic!("no fs-search answer within 4.5 s"));
    assert!(is_error(&response), "{response}");
    let text = batch_text(&response);
    assert!(text.contains("fancy-regex search stopped"), "{text}");
    assert!(text.contains("timeout_ms 1500 ms"), "{text}");
    assert!(elapsed < Duration::from_millis(4500), "{elapsed:?}");
    server.assert_responsive();
    std::fs::remove_dir_all(&dir).unwrap();
}

// 6. Worker results equal in-process results ----------------------------------
#[test]
fn fancy_results_match_in_process_and_only_fancy_items_spawn() {
    let dir = temp_dir("rust-fs-mcp-isolation-equal");
    let text = "foobar foobaz\n한글a 한글b 한글a\nx1 y2 x3\n";
    let file = dir.join("equal.txt");
    std::fs::write(&file, text).unwrap();
    let root = dir.display().to_string();
    let file_path = file.display().to_string();
    let fancy_search = json!({ "items": [{ "path": root, "pattern": r"foo(?=bar)|(?<=한글)a" }] });
    let fancy_replace = json!({ "items": [{
        "file_path": file_path,
        "pattern": r"(?<g>한글)(?=a)|(x)(?=\d)",
        "replacement": "<${g}$2>",
        "dryRun": true
    }] });
    let many_file = dir.join("many.txt");
    let many_text = "ab\n".repeat(MANY_LINES);
    std::fs::write(&many_file, &many_text).unwrap();
    let many_replace = json!({ "items": [{
        "file_path": many_file.display().to_string(),
        "pattern": "(a)(?=b)",
        "replacement": "$1$1"
    }] });

    // 1. 프로세스 내 기준 결과 (스폰 미활성)
    assert!(!isolate::worker_available());
    let search_local = dispatch_tool_call("fs-search", Some(fancy_search.clone()));
    let replace_local = dispatch_tool_call("file-replace", Some(fancy_replace.clone()));
    assert_eq!(status(&search_local), "success", "{search_local}");
    assert_eq!(status(&replace_local), "success", "{replace_local}");
    assert!(local_text(&search_local).contains("1:foobar foobaz"));
    assert!(local_text(&replace_local).contains("<한글>a"));
    let many_local = dispatch_tool_call("file-replace", Some(many_replace.clone()));
    assert_eq!(status(&many_local), "success", "{many_local}");
    let many_written = std::fs::read_to_string(&many_file).unwrap();
    assert_eq!(many_written, "aab\n".repeat(MANY_LINES));
    std::fs::write(&many_file, &many_text).unwrap();

    // 2. 서버 워커 결과 동일성
    let mut server = Server::start();
    let wait = Duration::from_secs(10);
    let search_served = server
        .call("fs-search", fancy_search.clone(), wait)
        .unwrap();
    let replace_served = server
        .call("file-replace", fancy_replace.clone(), wait)
        .unwrap();
    for (served, local) in [
        (&search_served, &search_local),
        (&replace_served, &replace_local),
    ] {
        assert_eq!(batch_text(served), local_text(local));
        let mut data = item_data(served);
        assert_eq!(data["isolated"], json!(true), "{served}");
        data.as_object_mut().unwrap().remove("isolated");
        assert_eq!(
            data,
            local["structuredContent"]["data"]["structuredContent"]["results"][0]["data"]
        );
    }
    let mut applied = fancy_replace.clone();
    applied["items"][0]["dryRun"] = json!(false);
    let written = server.call("file-replace", applied, wait).unwrap();
    assert!(!is_error(&written), "{written}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "foobar foobaz\n<한글>a 한글b <한글>a\n<x>1 y2 <x>3\n"
    );
    std::fs::write(&file, text).unwrap();

    // 3. 비fancy 항목 무스폰, fancy 항목만 스폰
    assert!(isolate::enable_worker_spawning_at(PathBuf::from(env!(
        "CARGO_BIN_EXE_rust-fs-mcp"
    ))));
    let before = isolate::spawn_count();
    for (tool, item) in [
        ("fs-search", json!({ "path": root, "pattern": "foo" })),
        (
            "fs-search",
            json!({ "path": root, "pattern": "(?=", "literal": true }),
        ),
        ("fs-search", json!({ "path": root, "pattern": "foo(" })),
        (
            "file-replace",
            json!({ "file_path": file_path, "pattern": r"(x)\d", "replacement": "$1", "dryRun": true }),
        ),
        (
            "file-replace",
            json!({ "file_path": file_path, "pattern": "(?=", "replacement": "", "mode": "literal", "dryRun": true }),
        ),
    ] {
        let response = dispatch_tool_call(tool, Some(json!({ "items": [item] })));
        assert_eq!(status(&response), "success", "{response}");
        assert!(
            response["structuredContent"]["data"]["structuredContent"]["results"][0]["data"]
                .get("isolated")
                .is_none(),
            "{response}"
        );
    }
    assert_eq!(isolate::spawn_count(), before);
    let search_spawned = dispatch_tool_call("fs-search", Some(fancy_search.clone()));
    assert_eq!(isolate::spawn_count(), before + 1);
    let replace_spawned = dispatch_tool_call("file-replace", Some(fancy_replace));
    assert_eq!(isolate::spawn_count(), before + 2);
    assert_eq!(local_text(&search_spawned), local_text(&search_local));
    assert_eq!(local_text(&replace_spawned), local_text(&replace_local));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);

    // 4. 응답 상한 초과 스팬 (출력 상한만 적용)
    let default_cap = isolate::set_reply_cap(SMALL_REPLY_CAP);
    let many_spawned = dispatch_tool_call("file-replace", Some(many_replace));
    isolate::set_reply_cap(default_cap);
    assert_eq!(isolate::spawn_count(), before + 3);
    assert_eq!(status(&many_spawned), "success", "{many_spawned}");
    // 본문 절단 위치만 다름 (isolated 필드 크기)
    let summary = format!("Replaced {MANY_LINES} match(es)");
    assert!(local_text(&many_spawned).contains(&summary));
    assert!(local_text(&many_local).contains(&summary));
    let mut data =
        many_spawned["structuredContent"]["data"]["structuredContent"]["results"][0]["data"]
            .clone();
    assert_eq!(data["replacements"], json!(MANY_LINES));
    assert_eq!(data["isolated"], json!(true));
    data.as_object_mut().unwrap().remove("isolated");
    assert_eq!(
        data,
        many_local["structuredContent"]["data"]["structuredContent"]["results"][0]["data"]
    );
    assert_eq!(std::fs::read_to_string(&many_file).unwrap(), many_written);

    // 5. 예열 워커 인수 작업 (결과 동일)
    let served = isolate::count_standby_jobs();
    let deadline = Instant::now() + Duration::from_secs(10);
    while isolate::count_standby_jobs() == served {
        assert!(Instant::now() < deadline, "no standby worker served a job");
        thread::sleep(Duration::from_millis(20));
        let search_warm = dispatch_tool_call("fs-search", Some(fancy_search.clone()));
        assert_eq!(local_text(&search_warm), local_text(&search_local));
        let item = "/structuredContent/data/structuredContent/results/0/data";
        let mut data = search_warm.pointer(item).cloned().unwrap_or_default();
        assert_eq!(data["isolated"], json!(true), "{search_warm}");
        data.as_object_mut().unwrap().remove("isolated");
        assert_eq!(Some(&data), search_local.pointer(item));
    }
    server.assert_responsive();
    std::fs::remove_dir_all(&dir).unwrap();
}

// 7. Worker process guards ----------------------------------------------------
fn start_worker(budget_ms: u64) -> Child {
    let mut worker = Command::new(env!("CARGO_BIN_EXE_rust-fs-mcp"))
        .arg(isolate::WORKER_FLAG)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn worker");
    let mut line = json!({
        "v": isolate::PROTOCOL_VERSION,
        "budgetMs": budget_ms,
        "job": { "op": "fancy-spans", "pattern": RUNAWAY_PATTERN, "text": "a".repeat(RUNAWAY_BYTES) }
    })
    .to_string();
    line.push('\n');
    let stdin = worker.stdin.as_mut().expect("worker stdin");
    stdin.write_all(line.as_bytes()).expect("write job");
    stdin.flush().expect("flush job");
    worker
}
// 종료 대기 (기한 초과 None)
fn wait_exit(worker: &mut Child, limit: Duration) -> Option<i32> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(status) = worker.try_wait().expect("try_wait") {
            return status.code();
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = worker.kill();
    let _ = worker.wait();
    None
}
#[test]
fn worker_exits_when_its_parent_goes_away_or_its_budget_passes() {
    // 1. 부모 종료 (stdin EOF)
    let mut orphan = start_worker(600_000);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(orphan.try_wait().expect("try_wait"), None);
    let started = Instant::now();
    drop(orphan.stdin.take());
    assert_eq!(wait_exit(&mut orphan, Duration::from_secs(5)), Some(5));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );

    // 2. 예산 + 2초 유예 (stdin 유지)
    let started = Instant::now();
    let mut late = start_worker(200);
    assert_eq!(wait_exit(&mut late, Duration::from_secs(6)), Some(3));
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(2200), "{elapsed:?}");
    drop(late.stdin.take());
}

// 8. Worker crash -------------------------------------------------------------
// 서버 자식 워커 강제 종료 (크래시 모사)
fn kill_worker_of(server_pid: u32) -> bool {
    let killed = if cfg!(windows) {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "$k = @(Get-CimInstance Win32_Process -Filter 'ParentProcessId={server_pid}'); $k | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}; exit [int]($k.Count -eq 0)"
                ),
            ])
            .stdin(Stdio::null())
            .output()
    } else {
        Command::new("pkill")
            .args(["-KILL", "-P", &server_pid.to_string()])
            .stdin(Stdio::null())
            .output()
    };
    killed.is_ok_and(|output| output.status.success())
}
#[test]
fn crashed_worker_fails_the_item_and_server_survives() {
    let dir = temp_dir("rust-fs-mcp-isolation-crash");
    let file = write_runaway_file(&dir);
    let before = std::fs::read(&file).unwrap();
    let mut server = Server::start();
    let server_pid = server.child.id();
    for (tool, item) in [
        (
            "fs-search",
            json!({ "path": dir.display().to_string(), "pattern": RUNAWAY_PATTERN, "multiline": true, "timeout_ms": 60_000 }),
        ),
        (
            "file-replace",
            json!({ "file_path": file.display().to_string(), "pattern": RUNAWAY_PATTERN, "replacement": "b", "timeout_ms": 60_000 }),
        ),
    ] {
        let id = server.send_request(
            "tools/call",
            json!({ "name": tool, "arguments": { "items": [item] } }),
        );

        // 1. 응답까지 워커 강제 종료 (예열 워커 포함)
        let deadline = Instant::now() + Duration::from_secs(20);
        let response = loop {
            assert!(
                Instant::now() < deadline,
                "{tool}: no answer after a worker kill"
            );
            if !kill_worker_of(server_pid) {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            if let Some(response) = server.wait_response(id, Duration::from_secs(1)) {
                break response;
            }
        };

        // 2. 항목 오류 응답과 서버 생존
        assert!(is_error(&response), "{response}");
        let text = batch_text(&response);
        assert!(text.contains("isolated worker crashed"), "{text}");
        server.assert_responsive();
    }
    assert_eq!(std::fs::read(&file).unwrap(), before);
    std::fs::remove_dir_all(&dir).unwrap();
}

// 9. Standby worker reuse -----------------------------------------------------
// 서버의 워커 자식 pid 목록
fn list_workers(server_pid: u32) -> Vec<u32> {
    let listed = if cfg!(windows) {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Get-CimInstance Win32_Process -Filter 'ParentProcessId={server_pid}' | Where-Object Name -eq 'rust-fs-mcp.exe' | ForEach-Object {{ $_.ProcessId }}"
                ),
            ])
            .stdin(Stdio::null())
            .output()
    } else {
        Command::new("pgrep")
            .args(["-P", &server_pid.to_string()])
            .stdin(Stdio::null())
            .output()
    };
    let listed = listed.expect("list worker processes");
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}
// 예열 워커 1개 대기 (이전 pid 제외)
fn wait_standby(server_pid: u32, previous: Option<u32>) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let workers = list_workers(server_pid);
        if let [worker] = workers[..]
            && Some(worker) != previous
        {
            return worker;
        }
        assert!(
            Instant::now() < deadline,
            "no idle standby worker: {workers:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}
// 프로세스 생존 여부 (좀비 제외)
fn check_process_alive(pid: u32) -> bool {
    if cfg!(windows) {
        let listed = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .stdin(Stdio::null())
            .output()
            .expect("tasklist");
        return String::from_utf8_lossy(&listed.stdout).contains(&format!("\"{pid}\""));
    }
    let listed = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .expect("ps");
    let state = String::from_utf8_lossy(&listed.stdout);
    listed.status.success() && !state.trim().is_empty() && !state.trim().starts_with('Z')
}
#[test]
fn standby_worker_serves_the_next_job_and_exits_with_the_server() {
    let dir = temp_dir("rust-fs-mcp-isolation-standby");
    std::fs::write(dir.join("standby.txt"), "foobar foobaz\n").unwrap();
    let search =
        json!({ "items": [{ "path": dir.display().to_string(), "pattern": "foo(?=bar)" }] });
    let mut server = Server::start();
    let server_pid = server.child.id();
    let wait = Duration::from_secs(10);

    // 1. 첫 작업 뒤 예열 기동
    let early = list_workers(server_pid);
    assert!(early.is_empty(), "worker before the first job: {early:?}");
    let first = server.call("fs-search", search.clone(), wait).unwrap();
    assert!(!is_error(&first), "{first}");
    assert!(batch_text(&first).contains("1:foobar foobaz"), "{first}");
    let standby = wait_standby(server_pid, None);

    // 2. 다음 작업의 예열 워커 인수
    let second = server.call("fs-search", search, wait).unwrap();
    assert_eq!(batch_text(&second), batch_text(&first));
    assert_eq!(item_data(&second), item_data(&first));
    assert_eq!(item_data(&second)["isolated"], json!(true), "{second}");
    let replacement = wait_standby(server_pid, Some(standby));
    assert!(
        !check_process_alive(standby),
        "standby {standby} was not used"
    );

    // 3. 서버 종료와 예열 워커 종료
    drop(server);
    let deadline = Instant::now() + Duration::from_secs(5);
    while check_process_alive(replacement) {
        assert!(
            Instant::now() < deadline,
            "standby {replacement} outlived its server"
        );
        thread::sleep(Duration::from_millis(100));
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

// 10. Standby worker timeout --------------------------------------------------
#[test]
fn timed_out_standby_job_is_killed_and_the_next_job_works() {
    let dir = temp_dir("rust-fs-mcp-isolation-standby-kill");
    let calm = dir.join("calm");
    let runaway = dir.join("runaway");
    std::fs::create_dir_all(&calm).unwrap();
    std::fs::create_dir_all(&runaway).unwrap();
    std::fs::write(calm.join("standby.txt"), "foobar foobaz\n").unwrap();
    write_runaway_file(&runaway);
    let search =
        json!({ "items": [{ "path": calm.display().to_string(), "pattern": "foo(?=bar)" }] });
    let mut server = Server::start();
    let server_pid = server.child.id();
    let wait = Duration::from_secs(10);

    // 1. 예열 워커 준비
    let first = server.call("fs-search", search.clone(), wait).unwrap();
    assert!(!is_error(&first), "{first}");
    let standby = wait_standby(server_pid, None);

    // 2. 예열 워커 폭주 작업 kill
    let started = Instant::now();
    let response = server.call(
        "fs-search",
        json!({ "items": [{
            "path": runaway.display().to_string(),
            "pattern": RUNAWAY_PATTERN,
            "multiline": true,
            "timeout_ms": 1500
        }] }),
        Duration::from_millis(4500),
    );
    let elapsed = started.elapsed();
    let response = response.unwrap_or_else(|| panic!("no fs-search answer within 4.5 s"));
    assert!(is_error(&response), "{response}");
    let text = batch_text(&response);
    assert!(text.contains("fancy-regex search stopped"), "{text}");
    assert!(elapsed < Duration::from_millis(4500), "{elapsed:?}");
    assert!(!check_process_alive(standby), "standby {standby} survived");

    // 3. 다음 작업 정상
    let third = server.call("fs-search", search, wait).unwrap();
    assert_eq!(batch_text(&third), batch_text(&first));
    assert_eq!(item_data(&third), item_data(&first));
    server.assert_responsive();
    std::fs::remove_dir_all(&dir).unwrap();
}
