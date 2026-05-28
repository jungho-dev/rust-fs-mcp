use crate::core::args_ref::read_text_slice;
use crate::core::batch::{create_batch_response, run_batch};
use crate::core::config::{default_shell, ensure_path_allowed, is_blocked_command};
use crate::core::response::RawResult;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct ProcSession {
    child: Child,
    stdin: Option<ChildStdin>,
    output: Arc<SharedOutput>,
    last_read: usize,
    command: String,
    shell: String,
    start_ms: u128,
    exit_code: Option<i32>,
}

static SESSIONS: OnceLock<Mutex<HashMap<i64, ProcSession>>> = OnceLock::new();

// 멀티 인스턴스 환경에서 한 호출이 다수 자식을 spawn 하면 Windows handle·reader 스레드가
// 누적 폭증한다. 한 호출이 동시에 띄울 수 있는 자식 수를 명시 cap 으로 제한한다.
const MAX_PROCESS_BATCH: usize = 16;

struct SharedOutput {
    text: Mutex<String>,
    changed: Condvar,
}

impl SharedOutput {
    fn new() -> Self {
        Self {
            text: Mutex::new(String::new()),
            changed: Condvar::new(),
        }
    }

    fn len(&self) -> usize {
        self.text.lock().unwrap().len()
    }

    fn snapshot(&self) -> String {
        self.text.lock().unwrap().clone()
    }

    fn slice_since(&self, start: usize) -> String {
        let output = self.text.lock().unwrap();
        slice_bytes_lossy(&output, start, None)
    }

    fn push_chunk(&self, chunk: &str, prefix: &str, line_start: &mut bool) {
        let mut output = self.text.lock().unwrap();
        if prefix.is_empty() {
            output.push_str(chunk);
        } else {
            append_prefixed_chunk(&mut output, chunk, prefix, line_start);
        }
        self.changed.notify_all();
    }

    fn wait_changed(&self, observed_len: usize, timeout: Duration) -> usize {
        let output = self.text.lock().unwrap();
        if output.len() != observed_len || timeout.is_zero() {
            return output.len();
        }
        let (output, _) = self.changed.wait_timeout(output, timeout).unwrap();
        output.len()
    }
}

// 1. Process tools ------------------------------------------------------------
pub fn handle_start_process(args: &Value) -> RawResult {
    let items = args
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![args.clone()]);
    if items.is_empty() {
        return RawResult::error("items must contain at least one process command");
    }
    if items.len() > MAX_PROCESS_BATCH {
        return RawResult::error(format!(
            "items exceeds per-call cap of {MAX_PROCESS_BATCH}; submit in smaller batches"
        ));
    }

    let results = run_batch(items, start_item);
    create_batch_response("start_process", results, false)
}

pub fn handle_read_process_output(args: &Value) -> RawResult {
    let items = args
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![args.clone()]);
    if items.is_empty() {
        return RawResult::error("items must contain at least one pid");
    }

    let results = run_batch(items, read_item);
    create_batch_response("read_process_output", results, true)
}

pub fn handle_kill_processes(args: &Value) -> RawResult {
    let mut items = Vec::new();
    if let Some(pids) = args.get("pids").and_then(Value::as_array) {
        items.extend(
            pids.iter()
                .filter_map(Value::as_i64)
                .map(|pid| json!({ "pid": pid })),
        );
    }
    if let Some(raw_items) = args.get("items").and_then(Value::as_array) {
        items.extend(raw_items.iter().cloned());
    }
    if let Some(pid) = args.get("pid").and_then(Value::as_i64) {
        items.push(json!({ "pid": pid }));
    }
    if items.is_empty() {
        return RawResult::error("pid, pids, or items is required");
    }

    let results = run_batch(items, kill_item);
    create_batch_response("kill_processes", results, false)
}

pub fn handle_list_processes(_args: &Value) -> RawResult {
    let mut sessions = sessions().lock().unwrap();
    refresh_all(&mut sessions);
    let list = sessions
        .iter()
        .map(|(pid, session)| {
            json!({
                "pid": pid,
                "command": session.command,
                "shell": session.shell,
                "startMs": session.start_ms,
                "exitCode": session.exit_code,
                "outputBytes": session.output.len()
            })
        })
        .collect::<Vec<_>>();

    RawResult::structured(
        format!("{} process sessions", list.len()),
        json!({ "sessions": list, "totalCount": list.len() }),
    )
}

pub fn handle_interact_with_processes(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items.clone(), interact_item);
    create_batch_response("interact_with_processes", results, false)
}

// 2. Start and interaction ----------------------------------------------------
fn start_item(item: Value) -> RawResult {
    let command_text = match command_input(&item) {
        Ok(command) => command,
        Err(error) => return RawResult::error(error),
    };
    if let Err(error) = validate_command(&command_text) {
        return RawResult::error(error);
    }

    let shell = item
        .get("shell")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(default_shell);
    let cwd = match item.get("cwd").and_then(Value::as_str) {
        Some(cwd) => match ensure_path_allowed(cwd) {
            Ok(cwd) => Some(cwd),
            Err(error) => return RawResult::error(error),
        },
        None => None,
    };
    let timeout_ms = usize_field(&item, "timeout_ms", 1000) as u64;

    let (program, args) = shell_invocation(&shell, &command_text);
    let mut command = Command::new(&program);
    command
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return RawResult::error(format!("Failed to start process: {error}")),
    };
    let pid = child.id() as i64;
    let output = Arc::new(SharedOutput::new());
    if let Some(stdout) = child.stdout.take() {
        spawn_reader(output.clone(), stdout, "");
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(output.clone(), stderr, "stderr: ");
    }
    let stdin = child.stdin.take();
    let start_ms = now_ms();

    sessions().lock().unwrap().insert(
        pid,
        ProcSession {
            child,
            stdin,
            output: output.clone(),
            last_read: 0,
            command: command_text,
            shell: shell.clone(),
            start_ms,
            exit_code: None,
        },
    );

    let snapshot = wait_for_output(pid, 0, timeout_ms);
    RawResult::structured(
        format!(
            "Process started with PID {pid}\nShell: {shell}\n\n{}",
            snapshot.text
        ),
        json!({
            "pid": pid,
            "shell": shell,
            "output": snapshot.text,
            "isComplete": snapshot.is_complete,
            "exitCode": snapshot.exit_code
        }),
    )
}

fn interact_item(item: Value) -> RawResult {
    let Some(pid) = item.get("pid").and_then(Value::as_i64) else {
        return RawResult::error("pid must be a number");
    };
    let input = match process_input(&item) {
        Ok(input) => input,
        Err(error) => return RawResult::error(error),
    };
    let timeout_ms = usize_field(&item, "timeout_ms", 8000) as u64;

    let before = {
        let mut sessions = sessions().lock().unwrap();
        refresh_all(&mut sessions);
        let Some(session) = sessions.get_mut(&pid) else {
            return RawResult::error(format!(
                "Process {pid} is not a managed rust-fs-mcp session"
            ));
        };
        if session.exit_code.is_some() {
            return RawResult::error(format!("Process {pid} has already exited"));
        }
        let before = session.output.len();
        let Some(stdin) = session.stdin.as_mut() else {
            return RawResult::error(format!("Process {pid} has no writable stdin"));
        };
        let mut input = input;
        if !input.ends_with('\n') {
            input.push('\n');
        }
        if let Err(error) = stdin.write_all(input.as_bytes()) {
            return RawResult::error(format!("Failed to write stdin for {pid}: {error}"));
        }
        if let Err(error) = stdin.flush() {
            return RawResult::error(format!("Failed to flush stdin for {pid}: {error}"));
        }
        before
    };

    let snapshot = wait_for_output(pid, before, timeout_ms);
    RawResult::structured(
        format!("Input sent to process {pid}\n{}", snapshot.text),
        json!({
            "pid": pid,
            "output": snapshot.text,
            "isComplete": snapshot.is_complete,
            "exitCode": snapshot.exit_code
        }),
    )
}

fn read_item(item: Value) -> RawResult {
    let Some(pid) = item.get("pid").and_then(Value::as_i64) else {
        return RawResult::error("pid must be a number");
    };
    let timeout_ms = usize_field(&item, "timeout_ms", 0) as u64;
    if timeout_ms > 0 {
        let current = output_len(pid).unwrap_or(0);
        let _ = wait_for_output(pid, current, timeout_ms);
    }

    let mut sessions = sessions().lock().unwrap();
    refresh_all(&mut sessions);
    let Some(session) = sessions.get_mut(&pid) else {
        return RawResult::error(format!(
            "Process {pid} is not a managed rust-fs-mcp session"
        ));
    };

    let output = session.output.snapshot();
    let offset = item.get("offset").and_then(Value::as_i64).unwrap_or(0);
    let length = item
        .get("length")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let start = if offset == 0 {
        session.last_read
    } else if offset < 0 {
        output.len().saturating_sub(offset.unsigned_abs() as usize)
    } else {
        offset as usize
    };
    let text = slice_bytes_lossy(&output, start, length);
    session.last_read = output.len();

    RawResult::structured(
        text.clone(),
        json!({
            "pid": pid,
            "output": text,
            "readFrom": start,
            "totalBytes": output.len(),
            "remaining": output.len().saturating_sub(start),
            "isComplete": session.exit_code.is_some(),
            "exitCode": session.exit_code
        }),
    )
}

fn kill_item(item: Value) -> RawResult {
    let Some(pid) = item.get("pid").and_then(Value::as_i64) else {
        return RawResult::error("pid must be a number");
    };

    let mut sessions = sessions().lock().unwrap();
    refresh_all(&mut sessions);
    let Some(session) = sessions.get_mut(&pid) else {
        return RawResult::error(format!(
            "Process {pid} is not a managed rust-fs-mcp session"
        ));
    };
    if session.exit_code.is_none() {
        if let Err(error) = session.child.kill() {
            return RawResult::error(format!("Failed to kill process {pid}: {error}"));
        }
        let _ = session.child.wait();
        session.exit_code = Some(-1);
    }

    RawResult::structured(
        format!("Killed process {pid}"),
        json!({ "pid": pid, "exitCode": session.exit_code }),
    )
}

// 3. Session helpers ----------------------------------------------------------
struct OutputSnapshot {
    text: String,
    is_complete: bool,
    exit_code: Option<i32>,
}

fn wait_for_output(pid: i64, start: usize, timeout_ms: u64) -> OutputSnapshot {
    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let output = {
        let sessions = sessions().lock().unwrap();
        match sessions.get(&pid) {
            Some(session) => session.output.clone(),
            None => {
                return OutputSnapshot {
                    text: String::new(),
                    is_complete: true,
                    exit_code: None,
                };
            }
        }
    };
    let mut last_len = start;
    let mut quiet_ticks = 0u8;

    loop {
        let (is_complete, exit_code) = {
            let mut sessions = sessions().lock().unwrap();
            refresh_all(&mut sessions);
            let Some(session) = sessions.get(&pid) else {
                return OutputSnapshot {
                    text: output.slice_since(start),
                    is_complete: true,
                    exit_code: None,
                };
            };
            (session.exit_code.is_some(), session.exit_code)
        };

        let len = output.len();
        if len > last_len {
            last_len = len;
            quiet_ticks = 0;
        } else if len > start {
            quiet_ticks = quiet_ticks.saturating_add(1);
        }
        if is_complete || quiet_ticks >= 2 || started.elapsed() >= timeout || timeout_ms == 0 {
            return OutputSnapshot {
                text: output.slice_since(start),
                is_complete,
                exit_code,
            };
        }
        // 50ms sleep 폴링 대신 출력 변경을 condvar 로 대기한다. 출력이 오면 즉시
        // 깨어나고, 정지 감지(quiet_ticks)를 위해 대기 상한은 50ms 로 둔다.
        let remaining = timeout.saturating_sub(started.elapsed());
        let wait = remaining.min(Duration::from_millis(50));
        output.wait_changed(len, wait);
    }
}

fn refresh_all(sessions: &mut HashMap<i64, ProcSession>) {
    for session in sessions.values_mut() {
        if session.exit_code.is_some() {
            continue;
        }
        if let Ok(Some(status)) = session.child.try_wait() {
            session.exit_code = Some(status.code().unwrap_or(-1));
            session.stdin = None;
        }
    }
}

fn sessions() -> &'static Mutex<HashMap<i64, ProcSession>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn spawn_reader<R>(output: Arc<SharedOutput>, reader: R, prefix: &'static str)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        let mut line_start = true;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    output.push_chunk(&line, prefix, &mut line_start);
                }
                Err(_) => break,
            }
        }
    });
}

fn append_prefixed_chunk(output: &mut String, chunk: &str, prefix: &str, line_start: &mut bool) {
    let mut remaining = chunk;
    while !remaining.is_empty() {
        if *line_start {
            output.push_str(prefix);
            *line_start = false;
        }
        match remaining.find('\n') {
            Some(index) => {
                output.push_str(&remaining[..=index]);
                *line_start = true;
                remaining = &remaining[index + 1..];
            }
            None => {
                output.push_str(remaining);
                break;
            }
        }
    }
}

fn output_len(pid: i64) -> Option<usize> {
    sessions()
        .lock()
        .unwrap()
        .get(&pid)
        .map(|session| session.output.text.lock().unwrap().len())
}

// 4. Argument helpers ---------------------------------------------------------
fn command_input(item: &Value) -> Result<String, String> {
    if let Some(command) = item.get("command").and_then(Value::as_str) {
        return Ok(command.to_string());
    }
    if let Some(path) = item.get("command_path").and_then(Value::as_str) {
        let path = ensure_path_allowed(path)?;
        let offset = item
            .get("command_offset")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let length = item
            .get("command_length")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        return read_text_slice(path, offset, length);
    }

    Err("command or command_path is required".to_string())
}

fn process_input(item: &Value) -> Result<String, String> {
    if let Some(input) = item.get("input").and_then(Value::as_str) {
        return Ok(input.to_string());
    }
    if let Some(path) = item.get("input_path").and_then(Value::as_str) {
        let path = ensure_path_allowed(path)?;
        let offset = item
            .get("input_offset")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let length = item
            .get("input_length")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        return read_text_slice(path, offset, length);
    }

    Err("input or input_path is required".to_string())
}

fn validate_command(command: &str) -> Result<(), String> {
    let first = command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches('"')
        .trim_matches('\'');
    if first.is_empty() {
        return Err("command must not be empty".to_string());
    }
    if is_blocked_command(first) {
        return Err(format!("Command not allowed: {first}"));
    }

    Ok(())
}

fn shell_invocation(shell: &str, command: &str) -> (String, Vec<String>) {
    let shell_path = PathBuf::from(shell);
    let shell_name = shell_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(shell)
        .to_ascii_lowercase();
    if shell_name == "cmd" || shell_name == "cmd.exe" {
        return (
            shell.to_string(),
            vec!["/C".to_string(), command.to_string()],
        );
    }
    if shell_name == "powershell"
        || shell_name == "powershell.exe"
        || shell_name == "pwsh"
        || shell_name == "pwsh.exe"
    {
        return (
            shell.to_string(),
            vec![
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ],
        );
    }

    (
        shell.to_string(),
        vec!["-c".to_string(), command.to_string()],
    )
}

fn slice_bytes_lossy(text: &str, start: usize, length: Option<usize>) -> String {
    // ASCII 가 대부분인 프로세스 출력에서 매 호출 char_indices 로 start 까지 전수 스캔하는
    // O(N) 비용을 피한다. start 가 이미 char 경계면 그대로 사용한다.
    let safe_start = if start >= text.len() {
        text.len()
    }
    else if text.is_char_boundary(start) {
        start
    }
    else {
        // 정밀 보정: start 이상에서 가장 가까운 char 경계로 이동.
        let mut index = start;
        while index < text.len() && !text.is_char_boundary(index) {
            index += 1;
        }
        index
    };
    let slice = &text[safe_start..];
    match length {
        Some(length) => slice.chars().take(length).collect(),
        None => slice.to_string(),
    }
}

fn usize_field(value: &Value, key: &str, default: usize) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_and_reads_managed_process() {
        let current_exe = std::env::current_exe().unwrap();
        let command = if cfg!(windows) {
            format!("\"{}\" --list", current_exe.display())
        } else {
            format!("'{}' --list", current_exe.display())
        };
        let start = handle_start_process(&json!({
            "command": command,
            "shell": if cfg!(windows) { "cmd.exe" } else { "sh" },
            "timeout_ms": 1000
        }));
        assert!(!start.is_error, "{start:?}");
        let pid = start.structured.unwrap()["results"][0]["result"]["structuredContent"]["pid"]
            .as_i64()
            .unwrap();

        let read = handle_read_process_output(&json!({ "pid": pid, "offset": -2000 }));
        assert!(!read.is_error, "{read:?}");
    }

    #[test]
    fn rejects_unmanaged_interaction() {
        let result = handle_interact_with_processes(&json!({
            "items": [{ "pid": 1, "input": "" }]
        }));
        assert!(result.is_error);
    }

    #[test]
    fn start_process_rejects_oversized_batch() {
        let mut items: Vec<Value> = Vec::with_capacity(MAX_PROCESS_BATCH + 1);
        for _ in 0..=MAX_PROCESS_BATCH {
            items.push(json!({ "command": "echo x" }));
        }
        let result = handle_start_process(&json!({ "items": items }));
        assert!(result.is_error, "expected cap to reject oversized batch");
        let text = result
            .content
            .iter()
            .filter_map(|content| content.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("exceeds per-call cap"), "text was: {text}");
    }
}
