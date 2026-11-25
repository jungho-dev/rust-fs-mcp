//! lines.rs
//! tools::fs::lines
//!
//! file-read-line-range handler: 1-based numbered line windows with UTF-16 and EUC-KR decoding.
//!

use super::shared::{OpenFailure, open_regular_file, read_items, usize_field};
use crate::core::batch::{READ_PLAN, create_batch_response, run_batch_parallel};
use crate::core::config::ensure_path_allowed;
use crate::core::encoding::{decode_bytes, decode_line, looks_utf16_prefix};
use crate::core::response::RawResult;
use crate::tools::args::common::bool_field;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

// 1. Line range tool ----------------------------------------------------------
pub fn handle_file_read_line_range(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let items = read_items(args);
    if items.is_empty() {
        return RawResult::error(
            "paths or items is required (e.g. {\"paths\":[\"C:/absolute/file\"]})",
        );
    }
    let results = run_batch_parallel(&items, READ_PLAN, move |item| {
        lines_item(item, allow_missing)
    });
    create_batch_response("file-read-line-range", results, true)
}
pub(super) fn lines_item(item: &Value, allow_missing: bool) -> RawResult {
    let allow_missing = allow_missing || bool_field(item, "allowMissing", false);
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let file = match open_regular_file(&path) {
        Ok((file, _)) => file,
        Err(OpenFailure::Missing) if allow_missing => {
            return RawResult::structured(
                format!("Missing path: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        Err(OpenFailure::Missing) => {
            return RawResult::error(format!("Path does not exist: {}", path.display()));
        }
        Err(OpenFailure::Directory) => {
            return RawResult::error(format!("Path is a directory: {}", path.display()));
        }
        Err(OpenFailure::Unreadable(error)) => {
            return RawResult::error(format!("Failed to read {}: {error}", path.display()));
        }
    };
    let start_line = usize_field(item, "start_line", 1);
    if start_line == 0 {
        return RawResult::error("start_line is 1-based and must be >= 1");
    }
    let line_count = item
        .get("line_count")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    if line_count == Some(0) {
        return RawResult::error("line_count must be >= 1");
    }
    // tail_lines: 마지막 N줄 (start_line/line_count 와 배타)
    let tail_lines = item
        .get("tail_lines")
        .map(|value| value.as_u64().filter(|count| *count >= 1));
    let tail_lines = match tail_lines {
        Some(Some(count)) => Some(count as usize),
        Some(None) => return RawResult::error("tail_lines must be an integer >= 1"),
        None => None,
    };
    if tail_lines.is_some()
        && (item.get("start_line").is_some() || item.get("line_count").is_some())
    {
        return RawResult::error("tail_lines cannot be combined with start_line or line_count");
    }
    let total_lines = match tail_lines.map(|_| count_total_lines(&path)).transpose() {
        Ok(total_lines) => total_lines,
        Err(error) => return RawResult::error(error),
    };
    let (start_line, line_count) = match (tail_lines, total_lines) {
        (Some(tail), Some(total)) => (total.saturating_sub(tail) + 1, Some(tail)),
        _ => (start_line, line_count),
    };
    let (body, returned, encoding) = match read_lines_native(file, &path, start_line, line_count) {
        Ok(read) => read,
        Err(error) => {
            return RawResult::error(error);
        }
    };

    let mut result = lines_result(&path, body, returned, Some("native-rust"));
    if let (Some(label), Some(structured)) = (encoding, result.structured.as_mut()) {
        structured["encoding"] = json!(label);
    }
    if let (Some(total), Some(structured)) = (total_lines, result.structured.as_mut()) {
        structured["totalLines"] = json!(total);
        structured["startLine"] = json!(start_line);
    }
    result
}
fn lines_result(path: &Path, body: String, returned: usize, backend: Option<&str>) -> RawResult {
    // The numbered body ships in text once; the previous structured.lines array re-sent every
    // line wrapped in {number,text} objects, more than doubling the payload.
    let mut structured = json!({
        "path": path.display().to_string(),
        "returned": returned
    });
    if let Some(backend) = backend {
        structured["backend"] = json!(backend);
    }
    let mut result = RawResult::structured(body, structured);
    if let Some(backend) = backend {
        result.meta.insert("backend".to_string(), json!(backend));
    }
    result
}
// 2. Native line scanner ------------------------------------------------------
// Builds the "{path}:\n{n}: {line}..." body in ONE pre-grown String while scanning: the
// previous Vec<(usize,String)> + per-line format! + join allocated three times per line.
fn read_lines_native(
    file: fs::File,
    path: &Path,
    start_line: usize,
    line_count: Option<usize>,
) -> Result<(String, usize, Option<&'static str>), String> {
    // 늦은 줄 번호를 찾을 때 BufReader 기본 8KB 버퍼는 4.5MB 파일에서 fill_buf/consume을
    // 550회 반복한다(실측 3.3ms). 256KB 버퍼로 반복을 18회 수준으로 줄인다.
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let mut line_number = 0usize;
    let path_text = path.display().to_string();
    // go-fs-mcp와 동일한 예상 용량: 제한된 요청은 줄당 ~24B, 무제한은 4KB에서 시작.
    let grow = match line_count {
        Some(limit) if limit <= 8192 => limit * 24,
        _ => 4096,
    };
    let mut body = String::with_capacity(path_text.len() + 2 + grow);
    body.push_str(&path_text);
    body.push_str(":\n");
    // UTF-16 전체 디코딩 후 줄 분할
    let head = reader
        .fill_buf()
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    if looks_utf16_prefix(head) {
        let decoded = decode_bytes(
            fs::read(path)
                .map_err(|error| format!("Failed to read {}: {error}", path.display()))?,
        );
        let label = decoded.legacy_label();
        let mut returned = 0usize;
        for (index, line) in decoded.text.lines().enumerate().skip(start_line - 1) {
            if line_count.is_some_and(|limit| returned >= limit) {
                break;
            }
            if returned > 0 {
                body.push('\n');
            }
            push_usize(&mut body, index + 1);
            body.push_str(": ");
            body.push_str(line);
            returned += 1;
        }
        return Ok((body, returned, label));
    }
    while line_number + 1 < start_line {
        let window = reader
            .fill_buf()
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if window.is_empty() {
            return Ok((body, 0, None));
        }
        let remaining = start_line - line_number - 1;
        let newlines = memchr::memchr_iter(b'\n', window).count();
        let consumed = if newlines >= remaining {
            memchr::memchr_iter(b'\n', window)
                .nth(remaining - 1)
                .unwrap()
                + 1
        } else {
            window.len()
        };
        line_number += newlines.min(remaining);
        reader.consume(consumed);
    }
    // 줄 단위 디코딩 (EUC-KR 폴백)
    let mut raw = Vec::new();
    let mut returned = 0usize;
    let mut legacy = false;
    loop {
        if let Some(line_count) = line_count
            && returned >= line_count
        {
            break;
        }
        raw.clear();
        let read = reader
            .read_until(b'\n', &mut raw)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        if raw.ends_with(b"\n") {
            raw.pop();
            if raw.ends_with(b"\r") {
                raw.pop();
            }
        }
        let (line, is_legacy) = decode_line(&raw);
        legacy |= is_legacy;
        if returned > 0 {
            body.push('\n');
        }
        push_usize(&mut body, line_number + 1);
        body.push_str(": ");
        body.push_str(&line);
        returned += 1;
        line_number += 1;
    }
    Ok((body, returned, legacy.then_some("EUC-KR")))
}
// tail_lines 기준 전체 줄 수: read_lines_native 와 같은 기준(마지막 개행 뒤 빈 줄 제외,
// UTF-16 은 전체 디코딩 후 lines()). 그 외는 256KB 버퍼로 개행만 스트리밍 집계한다.
fn count_total_lines(path: &Path) -> Result<usize, String> {
    let file = fs::File::open(path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let head = reader
        .fill_buf()
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    if looks_utf16_prefix(head) {
        let bytes = fs::read(path)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        return Ok(decode_bytes(bytes).text.lines().count());
    }
    let mut newlines = 0usize;
    let mut last_byte = None;
    loop {
        let window = reader
            .fill_buf()
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if window.is_empty() {
            break;
        }
        newlines += memchr::memchr_iter(b'\n', window).count();
        last_byte = window.last().copied();
        let consumed = window.len();
        reader.consume(consumed);
    }
    Ok(newlines + usize::from(last_byte.is_some_and(|byte| byte != b'\n')))
}
// 스택 버퍼 숫자 포매팅: 줄마다 fmt 기계를 타지 않는다(go strconv.AppendUint 대응).
fn push_usize(buf: &mut String, value: usize) {
    let mut digits = [0u8; 20];
    let mut index = digits.len();
    let mut rest = value;
    loop {
        index -= 1;
        digits[index] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    buf.push_str(std::str::from_utf8(&digits[index..]).unwrap_or("0"));
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::fs::make_temp_dir;

    #[test]
    fn read_line_windows_across_buffer_boundaries() {
        let dir = make_temp_dir("rust-fs-mcp-line-windows");
        let path = dir.join("probe.txt");
        let skipped_line = "한글".repeat(10_000);
        std::fs::write(
            &path,
            format!("{skipped_line}\r\n\r\n선택한 줄\r\n마지막 줄"),
        )
        .unwrap();
        for (start_line, expected) in [
            (2, "2: \n3: 선택한 줄"),
            (3, "3: 선택한 줄\n4: 마지막 줄"),
            (5, ""),
        ] {
            let file = fs::File::open(&path).unwrap();
            let (body, returned, _) = read_lines_native(file, &path, start_line, Some(2)).unwrap();
            assert_eq!(body, format!("{}:\n{expected}", path.display()));
            assert_eq!(returned, if start_line == 5 { 0 } else { 2 });
        }
        let file = fs::File::open(&path).unwrap();
        let (body, returned, _) = read_lines_native(file, &path, 4, None).unwrap();
        assert_eq!(body, format!("{}:\n4: 마지막 줄", path.display()));
        assert_eq!(returned, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn tail_lines_returns_last_lines_with_real_numbers() {
        let dir = make_temp_dir("rust-fs-mcp-tail-lines");
        let path = dir.join("ten.txt");
        let body: String = (1..=10).map(|number| format!("line{number}\n")).collect();
        std::fs::write(&path, body).unwrap();
        let path_text = path.display().to_string();

        // 마지막 3줄
        let tail = lines_item(&json!({ "path": path_text, "tail_lines": 3 }), false);
        assert!(!tail.is_error, "{tail:?}");
        assert_eq!(
            tail.content[0]["text"],
            format!("{path_text}:\n8: line8\n9: line9\n10: line10")
        );
        let structured = tail.structured.as_ref().unwrap();
        assert_eq!(structured["totalLines"], 10);
        assert_eq!(structured["startLine"], 8);
        assert_eq!(structured["returned"], 3);

        // 파일보다 긴 요청은 전체
        let whole = lines_item(&json!({ "path": path_text, "tail_lines": 50 }), false);
        assert_eq!(whole.structured.as_ref().unwrap()["returned"], 10);
        assert_eq!(whole.structured.as_ref().unwrap()["startLine"], 1);

        // 빈 파일은 0줄
        let empty_path = dir.join("empty.txt");
        std::fs::write(&empty_path, "").unwrap();
        let empty = lines_item(
            &json!({ "path": empty_path.display().to_string(), "tail_lines": 3 }),
            false,
        );
        assert!(!empty.is_error, "{empty:?}");
        assert_eq!(empty.structured.as_ref().unwrap()["returned"], 0);
        assert_eq!(empty.structured.as_ref().unwrap()["totalLines"], 0);

        // 잘못된 값과 동시 지정은 그 항목만 오류
        let zero = lines_item(&json!({ "path": path_text, "tail_lines": 0 }), false);
        assert!(format!("{zero:?}").contains("tail_lines must be an integer >= 1"));
        let batch = handle_file_read_line_range(&json!({ "items": [
            { "path": path_text, "tail_lines": 2, "start_line": 1 },
            { "path": path_text, "tail_lines": 2 }
        ] }));
        let summary = batch.structured.as_ref().unwrap();
        assert_eq!(summary["failedCount"], 1, "{batch:?}");
        assert_eq!(summary["succeededCount"], 1, "{batch:?}");
        assert!(
            format!("{batch:?}")
                .contains("tail_lines cannot be combined with start_line or line_count")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
