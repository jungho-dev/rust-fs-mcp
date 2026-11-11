//! lines.rs
//! tools::fs::lines
//!
//! file-read-line-range handler: 1-based numbered line windows with UTF-16 and EUC-KR decoding.
//!

use super::shared::{OpenFailure, open_regular_file, read_items, usize_field};
use crate::core::batch::{READ_PLAN, create_batch_response, run_batch_parallel};
use crate::core::config::ensure_path_allowed;
use crate::core::encoding::{decode_bytes, decode_line, has_utf16_bom};
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
    if has_utf16_bom(head) {
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
}
