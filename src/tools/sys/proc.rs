//! proc.rs
//! tools::sys::proc
//!
//! proc-list handler: runs %SystemRoot%\System32\tasklist.exe /FO CSV /NH with fixed arguments only.
//! Filters by name glob and PID inside the server; caller input never reaches the command line.
//! Decodes the raw bytes with decode_bytes so a CP949 session name stays readable.
//!

use crate::core::encoding::decode_bytes;
use crate::core::external::{ExternalTool, MAX_PIPE_BYTES, run_external_bytes};
use crate::core::response::RawResult;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::Duration;

// tasklist CSV 한 행
struct TaskRow {
    name: String,
    pid: u32,
    session: String,
    session_id: u32,
    mem_kb: u64,
}

// 1. Process list -------------------------------------------------------------
pub fn handle_proc_list(args: &Value) -> RawResult {
    // 1. Windows 전용
    if !cfg!(windows) {
        return RawResult::error("proc-list is only available on Windows");
    }

    // 2. 인자 검증
    let pattern = match args.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(name.to_lowercase()),
        Some(_) => return RawResult::error("name must be a string"),
    };
    let pids: Option<HashSet<u64>> = match args.get("pids") {
        None | Some(Value::Null) => None,
        Some(Value::Array(entries)) if entries.iter().all(Value::is_u64) => {
            Some(entries.iter().filter_map(Value::as_u64).collect())
        }
        Some(_) => return RawResult::error("pids must be an array of non-negative integers"),
    };
    let limit = args.get("maxResults").filter(|limit| !limit.is_null());
    let max_results = match limit.map(Value::as_u64) {
        None => 200,
        Some(Some(limit @ 1..=2000)) => limit as usize,
        Some(_) => return RawResult::error("maxResults must be between 1 and 2000"),
    };

    // 3. tasklist 실행 (고정 인자)
    let fixed_args = ["/FO", "CSV", "/NH"].map(str::to_string);
    let output = match run_external_bytes(
        ExternalTool::Tasklist,
        &fixed_args,
        None,
        Duration::from_secs(10),
    ) {
        Ok(output) => output,
        Err(error) => return RawResult::error(format!("proc-list: tasklist failed: {error}")),
    };
    if output.stdout_capped {
        return RawResult::error(format!(
            "proc-list: tasklist failed: output exceeded {MAX_PIPE_BYTES} bytes"
        ));
    }
    if output.status_code != Some(0) {
        let stderr = decode_bytes(output.stderr).text;
        return RawResult::error(format!(
            "proc-list: tasklist failed: exit {:?}: {}",
            output.status_code,
            stderr.trim()
        ));
    }

    // 4. 이름·PID 필터 (AND)
    let bare_pattern = pattern.as_deref().is_some_and(|text| !text.contains('.'));
    let mut matched: Vec<TaskRow> = Vec::new();
    for row in parse_task_rows(&decode_bytes(output.stdout).text) {
        if pids
            .as_ref()
            .is_some_and(|pids| !pids.contains(&u64::from(row.pid)))
        {
            continue;
        }
        let lowered = row.name.to_lowercase();
        let stem = lowered.strip_suffix(".exe").filter(|_| bare_pattern);
        if let Some(pattern) = &pattern
            && !matches_glob(pattern, &lowered)
            && !stem.is_some_and(|stem| matches_glob(pattern, stem))
        {
            continue;
        }
        matched.push(row);
    }

    // 5. 응답 구성
    let count = matched.len();
    let truncated = count > max_results;
    matched.truncate(max_results);
    let mut lines = vec!["PID\tNAME\tMEM_KB\tSESSION".to_string()];
    lines.extend(
        matched
            .iter()
            .map(|row| format!("{}\t{}\t{}\t{}", row.pid, row.name, row.mem_kb, row.session)),
    );
    if count == 0 {
        lines.push("0 processes".to_string());
    }
    let processes: Vec<Value> = matched
        .iter()
        .map(|row| {
            json!({
                "pid": row.pid,
                "name": row.name,
                "memKb": row.mem_kb,
                "session": row.session,
                "sessionId": row.session_id,
            })
        })
        .collect();
    RawResult::structured(
        lines.join("\n"),
        json!({ "count": count, "truncated": truncated, "processes": processes }),
    )
}
// CSV 행 해석 (따옴표 안 쉼표, 5열 외 행 제외)
fn parse_task_rows(text: &str) -> Vec<TaskRow> {
    let mut rows = Vec::new();
    for line in text.lines() {
        // 1. 따옴표 인식 열 분리
        let mut fields: Vec<String> = Vec::new();
        let mut field = String::new();
        let mut quoted = false;
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '"' if quoted && chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => quoted = !quoted,
                ',' if !quoted => fields.push(std::mem::take(&mut field)),
                _ => field.push(ch),
            }
        }
        fields.push(field);

        // 2. 열 변환 (메모리 숫자만)
        let [name, pid, session, session_id, memory] = fields.as_slice() else {
            continue;
        };
        let (Ok(pid), Ok(session_id)) =
            (pid.trim().parse::<u32>(), session_id.trim().parse::<u32>())
        else {
            continue;
        };
        let digits: String = memory.chars().filter(char::is_ascii_digit).collect();
        rows.push(TaskRow {
            name: name.clone(),
            pid,
            session: session.clone(),
            session_id,
            mem_kb: digits.parse().unwrap_or(0),
        });
    }
    rows
}
// 소문자 글롭 일치 (* ? 만 특수)
fn matches_glob(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let mut cursor = 0usize;
    let mut position = 0usize;
    let mut star: Option<(usize, usize)> = None;
    while position < name.len() {
        let wanted = pattern.get(cursor).copied();
        if wanted == Some('*') {
            star = Some((cursor, position));
            cursor += 1;
            continue;
        }
        if wanted.is_some_and(|ch| ch == '?' || ch == name[position]) {
            cursor += 1;
            position += 1;
            continue;
        }
        let Some((star_cursor, star_position)) = star else {
            return false;
        };
        cursor = star_cursor + 1;
        position = star_position + 1;
        star = Some((star_cursor, position));
    }
    pattern.iter().skip(cursor).all(|ch| *ch == '*')
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_rows_decode_cp949_session_and_memory() {
        let mut bytes = b"\"rust-fs-mcp.exe\",\"1234\",\"".to_vec();
        bytes.extend_from_slice(&[0xC4, 0xDC, 0xBC, 0xD6]);
        bytes.extend_from_slice(b"\",\"1\",\"12,345 K\"\r\n");
        let rows = parse_task_rows(&decode_bytes(bytes).text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "rust-fs-mcp.exe");
        assert_eq!(rows[0].pid, 1234);
        assert_eq!(rows[0].session, "콘솔");
        assert_eq!(rows[0].session_id, 1);
        assert_eq!(rows[0].mem_kb, 12345);
    }
    #[test]
    fn csv_rows_keep_quoted_commas_and_skip_short_lines() {
        let text =
            "\"a,b.exe\",\"7\",\"Console\",\"1\",\"1,024 K\"\n\"short.exe\",\"8\",\"Console\"\n\n";
        let rows = parse_task_rows(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "a,b.exe");
        assert_eq!(rows[0].pid, 7);
        assert_eq!(rows[0].mem_kb, 1024);
    }
    #[test]
    fn name_glob_matches_case_insensitive_and_exe_stem() {
        assert!(matches_glob(&"RUST-fs*".to_lowercase(), "rust-fs-mcp.exe"));
        assert!(matches_glob("rust-fs-mcp", "rust-fs-mcp"));
        assert!(!matches_glob("node", "rust-fs-mcp.exe"));
        assert!(!matches_glob("node", "rust-fs-mcp"));
        assert!(matches_glob("r?st-*-mcp.exe", "rust-fs-mcp.exe"));
        assert!(!matches_glob("rust", "rust-fs-mcp.exe"));
    }
    #[test]
    fn max_results_out_of_range_is_rejected() {
        for limit in [0, 2001] {
            let result = handle_proc_list(&json!({ "maxResults": limit }));
            assert!(result.is_error);
            let text = result.content[0]["text"].as_str().unwrap();
            let expected = if cfg!(windows) {
                "maxResults must be between 1 and 2000"
            } else {
                "proc-list is only available on Windows"
            };
            assert!(text.contains(expected), "{text}");
        }
    }
    #[cfg(windows)]
    #[test]
    fn live_tasklist_finds_the_current_test_process() {
        let exe = std::env::current_exe().unwrap();
        let exe_name = exe.file_name().unwrap().to_string_lossy().into_owned();
        let result = handle_proc_list(&json!({ "pids": [std::process::id()] }));
        assert!(!result.is_error, "{:?}", result.content);
        let structured = result.structured.unwrap();
        assert_eq!(structured["count"], 1);
        assert_eq!(structured["truncated"], false);
        assert_eq!(structured["processes"][0]["name"], exe_name.as_str());
        assert_eq!(structured["processes"][0]["pid"], std::process::id());
        // 이름 글롭과 AND
        let stem = exe_name.strip_suffix(".exe").unwrap().to_uppercase();
        let both = handle_proc_list(&json!({ "name": stem, "pids": [std::process::id()] }));
        assert_eq!(both.structured.unwrap()["count"], 1);
        let none = handle_proc_list(&json!({ "name": "node", "pids": [std::process::id()] }));
        let none_text = none.content[0]["text"].as_str().unwrap().to_string();
        assert_eq!(none.structured.unwrap()["count"], 0);
        assert!(none_text.ends_with("0 processes"), "{none_text}");
    }
}
