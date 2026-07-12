//! search_tools.rs
//! tools::search_tools
//!
//! Content regex search tool: fs-search.
//! Calls ripgrep from PATH as an external command and returns each batch result directly.
//!

use crate::core::args_ref::read_text_slice;
use crate::core::batch::{SEARCH_PLAN, create_batch_response, run_batch_parallel};
use crate::core::config::ensure_path_allowed;
use crate::core::external::ExternalTool;
use crate::core::response::RawResult;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

#[derive(Clone, Debug)]
struct SearchSession {
    lines: Vec<String>,
    backend: String,
}

#[derive(Deserialize)]
struct RgEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    data: Option<RgData>,
}

#[derive(Deserialize)]
struct RgData {
    path: Option<RgText>,
    line_number: Option<u64>,
    lines: Option<RgText>,
}

#[derive(Deserialize)]
struct RgText {
    text: String,
}

// 1. Search tool --------------------------------------------------------------
pub fn handle_fs_search(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_parallel(items, SEARCH_PLAN, regex_item);
    create_batch_response("fs-search", results, true)
}

fn regex_item(item: &Value) -> RawResult {
    let search = match run_regex_search(item) {
        Ok(search) => search,
        Err(error) => return RawResult::error(error),
    };
    // The match lines ship in the text body once; structured carries metadata only instead of
    // re-sending the same lines as a JSON array.
    let text = search.lines.join("\n");
    let backend = search.backend;
    let total = search.lines.len();
    let mut result = RawResult::structured(
        text,
        json!({
            "backend": &backend,
            "totalCount": total
        }),
    );
    result.meta.insert("backend".to_string(), json!(backend));
    result
}

// 2. External search runner ----------------------------------------------------
fn run_regex_search(item: &Value) -> Result<SearchSession, String> {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return Err("path must be a string".to_string());
    };
    let path = ensure_path_allowed(path)?;
    let pattern = read_pattern(item)?;
    let include_hidden = bool_field(item, "includeHidden", false);
    let file_pattern = item.get("filePattern").and_then(Value::as_str);
    let max_results = item
        .get("maxResults")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(usize::MAX);
    if max_results == 0 {
        return Ok(SearchSession {
            lines: Vec::new(),
            backend: ExternalTool::Rg.backend_name().to_string(),
        });
    }
    // 기본 타임아웃은 Codex 계열 tools/call 30초 상한 안쪽이면서 대형 트리를 감당하는 25초.
    run_rg_search(RgOpts {
        path: &path,
        pattern: &pattern,
        item,
        include_hidden,
        file_pattern,
        max_results,
        context_default: 2,
        timeout_default: Some(25_000),
        fixed_strings: false,
    })
}

struct RgOpts<'a> {
    path: &'a Path,
    pattern: &'a str,
    item: &'a Value,
    include_hidden: bool,
    file_pattern: Option<&'a str>,
    max_results: usize,
    context_default: usize,
    timeout_default: Option<u64>,
    fixed_strings: bool,
}

fn run_rg_search(opts: RgOpts<'_>) -> Result<SearchSession, String> {
    let context = opts
        .item
        .get("contextLines")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(opts.context_default);
    let timeout_ms = opts
        .item
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .or(opts.timeout_default);
    let mut args = vec![
        "--json".to_string(),
        "--line-number".to_string(),
        "--color=never".to_string(),
        "--no-ignore".to_string(),
    ];
    if bool_field(opts.item, "ignoreCase", true) {
        args.push("--ignore-case".to_string());
    }
    if opts.include_hidden {
        args.push("--hidden".to_string());
    }
    if context > 0 {
        args.push("--context".to_string());
        args.push(context.to_string());
    }
    for pattern in split_patterns(opts.file_pattern) {
        args.push("--glob".to_string());
        args.push(pattern);
    }
    // 기본 제외: 대형 산출물 디렉터리(node_modules/target, hidden 시 .git)가 대형 트리
    // 타임아웃의 주범이라 사용자 glob 뒤에 push 해 우선 적용한다(rg는 마지막 매치 우선).
    // 검색 루트가 그 내부이거나 noDefaultExcludes:true 면 적용하지 않는다.
    if !bool_field(opts.item, "noDefaultExcludes", false) && !path_in_heavy_dir(opts.path) {
        for pattern in ["!**/node_modules/**", "!**/target/**"] {
            args.push("--glob".to_string());
            args.push(pattern.to_string());
        }
        if opts.include_hidden {
            args.push("--glob".to_string());
            args.push("!**/.git/**".to_string());
        }
    }
    // maxResults 가 유한하면 파일당 매치 상한(-m)으로 병리적 대용량 파일 스캔을 자른다.
    if opts.max_results != usize::MAX {
        args.push("--max-count".to_string());
        args.push(opts.max_results.to_string());
    }
    if opts.fixed_strings {
        args.push("--fixed-strings".to_string());
    }
    args.push("--".to_string());
    args.push(opts.pattern.to_string());
    args.push(opts.path.display().to_string());

    let backend = ExternalTool::Rg.backend_name();
    let stream = stream_rg(&args, timeout_ms.unwrap_or(120_000), opts.max_results).map_err(|error| {
        if error.contains("timed out") {
            format!("{error}; narrow the search path, add filePattern, or raise timeout_ms")
        } else {
            error
        }
    })?;
    // maxResults 조기 종료는 정상 부분 성공(go의 cancel-and-return과 동일).
    if stream.killed_early {
        return Ok(SearchSession {
            lines: stream.lines,
            backend: backend.to_string(),
        });
    }
    if !matches!(stream.status_code, Some(0) | Some(1)) {
        let stderr = stream.stderr.trim().to_string();
        // 정규식 파스 실패는 리터럴 검색으로 1회 폴백해 오류 대신 결과를 돌려준다.
        if !opts.fixed_strings
            && (stderr.contains("regex parse error") || stderr.contains("not allowed in a regex"))
        {
            let mut session = run_rg_search(RgOpts {
                fixed_strings: true,
                ..opts
            })?;
            session
                .backend
                .push_str(" (literal fallback: regex parse error)");
            return Ok(session);
        }
        // 종료코드 2라도 매치가 있으면(잠긴 파일 등 일부 실패) 부분 결과로 살린다.
        if stream.status_code == Some(2) && !stream.lines.is_empty() {
            return Ok(SearchSession {
                lines: stream.lines,
                backend: format!("{backend} (partial: some files were unreadable)"),
            });
        }
        return Err(format!(
            "external rg failed with code {:?}: {stderr}",
            stream.status_code
        ));
    }

    Ok(SearchSession {
        lines: stream.lines,
        backend: backend.to_string(),
    })
}

// 2a. Streaming rg runner -------------------------------------------------------
// rg stdout을 전량 버퍼링하던 경로를 줄 단위 스트림 디코드로 교체: maxResults 도달 시
// 프로세스를 즉시 종료해 잔여 트리 스캔을 생략하고(go streaming decode + cancel 동일),
// 대기 중 차단은 watchdog 스레드가 데드라인에 kill로 해제한다.
struct RgStream {
    lines: Vec<String>,
    status_code: Option<i32>,
    stderr: String,
    killed_early: bool,
}

fn stream_rg(args: &[String], timeout_ms: u64, max_results: usize) -> Result<RgStream, String> {
    let backend = ExternalTool::Rg.backend_name();
    let mut command = Command::new("rg");
    command.args(args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        format!(
            "Failed to start {backend} from PATH: {error}. Ensure 'rg' is installed and on PATH."
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture stderr".to_string())?;
    let stderr_handle = thread::spawn(move || {
        let mut pipe = stderr;
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    });
    let child = Arc::new(Mutex::new(child));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog_child = Arc::clone(&child);
    let timeout = Duration::from_millis(timeout_ms);
    let watchdog = thread::spawn(move || {
        if done_rx.recv_timeout(timeout).is_err() {
            if let Ok(mut child) = watchdog_child.lock() {
                let _ = child.kill();
            }
            return true;
        }
        false
    });

    let mut reader = BufReader::new(stdout);
    let mut raw_line = String::new();
    let mut lines = Vec::new();
    let mut hits = 0usize;
    let mut last_path = String::new();
    let mut killed_early = false;
    let mut parse_error: Option<String> = None;
    loop {
        raw_line.clear();
        let read = match reader.read_line(&mut raw_line) {
            Ok(read) => read,
            // kill/timeout 이후 파이프 오류는 EOF로 취급.
            Err(_) => break,
        };
        if read == 0 {
            break;
        }
        if let Err(error) = append_rg_event(
            raw_line.trim_end_matches(['\r', '\n']),
            &mut lines,
            &mut hits,
            &mut last_path,
        ) {
            parse_error = Some(error);
            break;
        }
        if hits >= max_results {
            killed_early = true;
            break;
        }
    }
    // 조기 종료·파스 오류로 루프를 떠난 경우 파이프가 차서 wait()가 막히지 않도록 먼저 kill.
    if killed_early || parse_error.is_some() {
        if let Ok(mut child) = child.lock() {
            let _ = child.kill();
        }
    }
    let _ = done_tx.send(());
    let timed_out = watchdog.join().unwrap_or(false);
    let stderr_bytes = stderr_handle.join().unwrap_or_default();
    let status_code = child
        .lock()
        .ok()
        .and_then(|mut child| child.wait().ok())
        .and_then(|status| status.code());
    if timed_out && !killed_early {
        return Err(format!("{backend} timed out after {timeout_ms}ms"));
    }
    if let Some(error) = parse_error
        && !killed_early
    {
        return Err(error);
    }
    Ok(RgStream {
        lines,
        status_code: if killed_early { Some(0) } else { status_code },
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        killed_early,
    })
}

// 한 줄의 rg --json 이벤트를 출력 버퍼에 반영(스트림·일괄 파스 공용).
fn append_rg_event(
    line: &str,
    lines: &mut Vec<String>,
    hits: &mut usize,
    last_path: &mut String,
) -> Result<(), String> {
    if line.is_empty() {
        return Ok(());
    }
    let event = serde_json::from_str::<RgEvent>(line)
        .map_err(|error| format!("Failed to parse external rg JSON: {error}"))?;
    let Some(event_type) = event.event_type.as_deref() else {
        return Ok(());
    };
    if event_type != "match" && event_type != "context" {
        return Ok(());
    }
    let Some(data) = event.data else {
        return Ok(());
    };
    let Some(path) = data.path.as_ref().map(|path| path.text.as_str()) else {
        return Ok(());
    };
    let line_number = data.line_number.unwrap_or(0);
    let text = data
        .lines
        .as_ref()
        .map(|lines| lines.text.as_str())
        .unwrap_or_default()
        .trim_end_matches(['\r', '\n']);
    // Emit the file path once per file run (rg --heading style) instead of repeating it on every result line.
    if path != *last_path {
        last_path.clear();
        last_path.push_str(path);
        lines.push(path.to_string());
    }
    let sep = if event_type == "match" { ":" } else { "-" };
    lines.push(format!("{line_number}{sep}{text}"));
    *hits += 1;
    Ok(())
}

#[cfg(test)]
fn parse_rg_json(stdout: &str, max_results: usize) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    let mut hits = 0usize;
    let mut last_path = String::new();
    for line in stdout.lines() {
        if hits >= max_results {
            break;
        }
        append_rg_event(line, &mut lines, &mut hits, &mut last_path)?;
    }
    Ok(lines)
}

// 검색 루트가 기본 제외 대상 디렉터리 내부인지 판별(명시 탐색 의도는 존중).
fn path_in_heavy_dir(path: &std::path::Path) -> bool {
    path.components().any(|component| {
        let text = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        text == "node_modules" || text == "target" || text == ".git"
    })
}

fn split_patterns(pattern: Option<&str>) -> Vec<String> {
    pattern
        .into_iter()
        .flat_map(|value| value.split('|'))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn read_pattern(item: &Value) -> Result<String, String> {
    if let Some(path) = item.get("pattern_path").and_then(Value::as_str) {
        let path = ensure_path_allowed(path)?;
        let offset = item
            .get("pattern_offset")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let length = item
            .get("pattern_length")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        return read_text_slice(path, offset, length);
    }

    // 다른 도구 인자를 들고 온 혼동 호출은 교정 힌트로 안내.
    if item.get("start_line").is_some() || item.get("line_count").is_some() {
        return Err(
            "pattern is required; fs-search matches regex content — for start_line/line_count reads use file-read-line-range"
                .to_string(),
        );
    }
    item.get("pattern")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "pattern or pattern_path is required".to_string())
}

fn bool_field(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_pattern_is_error() {
        let result = handle_fs_search(&json!({ "items": [{ "path": "." }] }));
        assert!(result.is_error);
    }

    #[test]
    fn line_range_args_get_targeted_hint() {
        let result = handle_fs_search(
            &json!({ "items": [{ "path": ".", "start_line": 3, "line_count": 2 }] }),
        );
        assert!(result.is_error);
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("file-read-line-range"), "{text}");
    }

    #[test]
    fn invalid_regex_falls_back_to_literal() {
        // rg 필요(tool_matrix와 동일 전제). "sendCancel(" 는 파스 오류 → 리터럴 폴백 매치.
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-litfb-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sample.txt"), "call sendCancel( now\n").unwrap();

        let result = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "sendCancel(" }]
        }));
        assert!(!result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("sendCancel("), "{text}");
        let structured = result.structured.clone().unwrap();
        let backend = structured["results"][0]["data"]["backend"]
            .as_str()
            .unwrap_or_default();
        assert!(backend.contains("literal fallback"), "{backend}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_rg_json_groups_lines_under_file_headings() {
        let stdout = concat!(
            "{\"type\":\"begin\",\"data\":{\"path\":{\"text\":\"C:\\\\repo\\\\a.rs\"}}}\n",
            "{\"type\":\"context\",\"data\":{\"path\":{\"text\":\"C:\\\\repo\\\\a.rs\"},\"line_number\":1,\"lines\":{\"text\":\"ctx\\n\"}}}\n",
            "{\"type\":\"match\",\"data\":{\"path\":{\"text\":\"C:\\\\repo\\\\a.rs\"},\"line_number\":2,\"lines\":{\"text\":\"hit\\n\"}}}\n",
            "{\"type\":\"match\",\"data\":{\"path\":{\"text\":\"C:\\\\repo\\\\b.rs\"},\"line_number\":7,\"lines\":{\"text\":\"hit2\\n\"}}}\n"
        );
        let lines = parse_rg_json(stdout, usize::MAX).unwrap();
        assert_eq!(
            lines,
            vec![
                "C:\\repo\\a.rs".to_string(),
                "1-ctx".to_string(),
                "2:hit".to_string(),
                "C:\\repo\\b.rs".to_string(),
                "7:hit2".to_string(),
            ]
        );

        // maxResults counts match/context lines only, never headings.
        let capped = parse_rg_json(stdout, 1).unwrap();
        assert_eq!(
            capped,
            vec!["C:\\repo\\a.rs".to_string(), "1-ctx".to_string()]
        );
    }
}
