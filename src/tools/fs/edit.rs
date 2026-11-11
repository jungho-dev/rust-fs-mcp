//! edit.rs
//! tools::fs::edit
//!
//! file-edit (exact block replace) and file-edit-lines (1-based line range replace) handlers.
//! Both keep the file's legacy encoding and dominant EOL style.
//! dryRun:true runs every validation and the encode, then returns the unified diff instead of writing.
//!

use super::shared::{
    OpenFailure, edit_lines_touched, edit_touched, open_regular_file, window_line_count,
};
use super::write::write_atomic;
use crate::core::batch::{create_batch_response, run_batch_mutation};
use crate::core::config::ensure_path_allowed;
use crate::core::diff::{DiffSpec, DiffSummary, render_unified_diff};
use crate::core::encoding::{decode_bytes, encode_text};
use crate::core::response::RawResult;
use crate::tools::args::common::{bool_field, item_content};
use serde_json::{Value, json};
use std::io::Read;

// 1. Edit tools ---------------------------------------------------------------
pub fn handle_file_edit(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    // diff 줄바꿈 보존 (dryRun 한정)
    let full = items.iter().any(|item| bool_field(item, "dryRun", false));
    let results = run_batch_mutation(items, edit_touched, edit_item);
    create_batch_response("file-edit", results, full)
}
pub fn handle_file_edit_lines(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let full = items.iter().any(|item| bool_field(item, "dryRun", false));
    let results = run_batch_mutation(items, edit_lines_touched, edit_lines_item);
    create_batch_response("file-edit-lines", results, full)
}
pub(super) fn edit_item(item: &Value) -> RawResult {
    let Some(path) = item.get("file_path").and_then(Value::as_str) else {
        return RawResult::error("file_path must be a string");
    };
    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let mut file = match open_regular_file(&path) {
        Ok((file, _)) => file,
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
    let old_string = match item_content(item, "old_string", "old_string_path") {
        Ok(content) => content,
        Err(error) => return RawResult::error(error),
    };
    let new_string = match item_content(item, "new_string", "new_string_path") {
        Ok(content) => content,
        Err(error) => return RawResult::error(error),
    };
    // 쓰기 전 핸들 반납
    let mut bytes = Vec::new();
    let read = file.read_to_end(&mut bytes);
    drop(file);
    if let Err(error) = read {
        return RawResult::error(format!("Failed to read {}: {error}", path.display()));
    }
    // 레거시 인코딩 보존 편집
    let decoded = decode_bytes(bytes);
    if decoded.lossy {
        return RawResult::error(format!(
            "{} is not valid UTF-8, UTF-16, or EUC-KR text; refusing to edit",
            path.display()
        ));
    }
    let text = decoded.text.as_str();

    let (effective_old, effective_new, eol_mode) =
        resolve_edit_strings(text, &old_string, &new_string);
    // 빈 old_string은 모든 문자 경계에 삽입되어 파일 전체를 손상시키므로 거부.
    if effective_old.is_empty() {
        return RawResult::error("old_string must not be empty");
    }
    // Single pass: build the edited body while counting matches, instead of scanning the
    // whole text twice via matches().count() followed by replace().
    let mut edited = String::with_capacity(text.len());
    let mut match_lines = Vec::new();
    let mut line_number = 1usize;
    let mut rest = text;
    while let Some(found) = rest.find(&effective_old) {
        line_number += memchr::memchr_iter(b'\n', &rest.as_bytes()[..found]).count();
        match_lines.push(line_number);
        line_number += memchr::memchr_iter(b'\n', effective_old.as_bytes()).count();
        edited.push_str(&rest[..found]);
        edited.push_str(&effective_new);
        rest = &rest[found + effective_old.len()..];
    }
    edited.push_str(rest);
    let count = match_lines.len();
    if count == 0 {
        return RawResult::error("old_string was not found");
    }
    // 미지정 다중 매치 거부 (스키마 기본값 1)
    let expected = item
        .get("expected_replacements")
        .and_then(Value::as_u64)
        .unwrap_or(1) as usize;
    if count != expected {
        let listed: Vec<String> = match_lines.iter().take(10).map(usize::to_string).collect();
        return RawResult::error(format!(
            "Expected {expected} replacement(s) but found {count} (lines {}{}); add surrounding context to old_string or set expected_replacements:{count}",
            listed.join(", "),
            if count > 10 { ", ..." } else { "" }
        ));
    }
    // 인코딩 불가 문자도 dryRun 검증 대상
    let dry_run = bool_field(item, "dryRun", false);
    let encoded = match encode_text(&edited, decoded.encoding, decoded.bom) {
        Ok(encoded) => encoded,
        Err(error) => {
            return RawResult::error(format!("Failed to encode {}: {error}", path.display()));
        }
    };
    if !dry_run && let Err(error) = write_atomic(&path, &encoded) {
        return RawResult::error(format!("Failed to write {}: {error}", path.display()));
    }
    let mut structured = json!({
        "file_path": path.display().to_string(),
        "replacements": count,
        "bytes": encoded.len(),
        "eolMode": eol_mode
    });
    if let Some(label) = decoded.legacy_label() {
        structured["encoding"] = json!(label);
    }
    if dry_run {
        let headline = format!("Would edit {} ({count} replacements)", path.display());
        return preview_result(text, &edited, headline, structured);
    }
    RawResult::structured(
        format!("Edited {} ({count} replacements)", path.display()),
        structured,
    )
}
fn edit_lines_item(item: &Value) -> RawResult {
    let Some(path) = item.get("file_path").and_then(Value::as_str) else {
        return RawResult::error("file_path must be a string");
    };
    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let mut file = match open_regular_file(&path) {
        Ok((file, _)) => file,
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
    let Some(start_line) = item.get("start_line").and_then(Value::as_u64) else {
        return RawResult::error("start_line must be a positive integer");
    };
    if start_line == 0 {
        return RawResult::error("start_line is 1-based and must be >= 1");
    }
    let end_line = item
        .get("end_line")
        .and_then(Value::as_u64)
        .unwrap_or(start_line);
    if end_line < start_line {
        return RawResult::error("end_line must be >= start_line");
    }
    let after = bool_field(item, "after", false);
    let replacement = match item.get("replacement").and_then(Value::as_str) {
        Some(value) => value.to_string(),
        None => match item_content(item, "replacement", "replacement_path") {
            Ok(value) => value,
            Err(error) => return RawResult::error(error),
        },
    };

    // 쓰기 전 핸들 반납
    let mut bytes = Vec::new();
    let read = file.read_to_end(&mut bytes);
    drop(file);
    if let Err(error) = read {
        return RawResult::error(format!("Failed to read {}: {error}", path.display()));
    }
    let decoded = decode_bytes(bytes);
    if decoded.lossy {
        return RawResult::error("file is not valid UTF-8, UTF-16, or EUC-KR text");
    }
    let text = decoded.text.as_str();

    let eol = detect_dominant_eol(text);
    let total_lines = window_line_count(text);

    // expected_lines는 파일 전체 줄 수 드리프트 가드. 실사용 오류 전수가 "교체 범위 길이"로
    // 값을 준 경우라 범위 길이(end_line-start_line+1) 일치도 통과시킨다.
    if let Some(expected) = item.get("expected_lines").and_then(Value::as_u64)
        && total_lines as u64 != expected
        && end_line - start_line + 1 != expected
    {
        return RawResult::error(format!(
            "Expected {expected} lines but file has {total_lines} (expected_lines is the whole-file line count; the selected range {start_line}-{end_line} spans {} lines)",
            end_line - start_line + 1
        ));
    }
    if start_line as usize > total_lines && !after {
        return RawResult::error(format!(
            "start_line {start_line} exceeds file line count {total_lines}"
        ));
    }
    let effective_end = (end_line as usize).min(total_lines.max(1));
    let start_idx = start_line as usize - 1;
    let end_idx = effective_end.saturating_sub(1);
    // start_idx/end_idx 두 줄의 byte range만 찾는다(전체 라인 Vec 미생성). after 모드에서
    // end_idx가 total_lines를 벗어날 수 있어(파일 끝 너머 삽입) 범위 안으로 clamp해 호출한다.
    let (start_range, end_range) = if total_lines == 0 {
        ((0, 0), (0, 0))
    } else {
        line_range_pair(
            text,
            start_idx.min(total_lines - 1),
            end_idx.min(total_lines - 1),
        )
    };

    let normalized = normalize_replacement_eol(&replacement, eol);
    let trailing_eol_needed = !normalized.is_empty() && !ends_with_eol(&normalized);
    // replace 모드: 교체 대상이 EOL로 끝날 때만 EOL 보존. 후행 개행 없는 마지막 줄 교체 시 추가 금지.
    let append_eol = if after {
        true
    } else {
        text[..end_range.1].ends_with('\n')
    };
    let final_replacement = if trailing_eol_needed && append_eol {
        let mut value = normalized;
        value.push_str(eol);
        value
    } else {
        normalized
    };

    let mut new_text = String::with_capacity(text.len() + final_replacement.len());
    if after {
        let insert_byte = if total_lines == 0 {
            0
        } else if end_idx < total_lines {
            end_range.1
        } else {
            text.len()
        };
        new_text.push_str(&text[..insert_byte]);
        let needs_eol_before = insert_byte > 0
            && !text[..insert_byte].ends_with('\n')
            && !final_replacement.is_empty();
        if needs_eol_before {
            new_text.push_str(eol);
        }
        new_text.push_str(&final_replacement);
        new_text.push_str(&text[insert_byte..]);
    } else {
        let cut_start = start_range.0;
        let cut_end = end_range.1;
        new_text.push_str(&text[..cut_start]);
        new_text.push_str(&final_replacement);
        new_text.push_str(&text[cut_end..]);
    }
    let dry_run = bool_field(item, "dryRun", false);
    let encoded = match encode_text(&new_text, decoded.encoding, decoded.bom) {
        Ok(encoded) => encoded,
        Err(error) => {
            return RawResult::error(format!("Failed to encode {}: {error}", path.display()));
        }
    };
    if !dry_run && let Err(error) = write_atomic(&path, &encoded) {
        return RawResult::error(format!("Failed to write {}: {error}", path.display()));
    }
    let action = if after {
        "insert_after"
    } else if final_replacement.is_empty() {
        "delete"
    } else {
        "replace"
    };
    let lines_changed = if after {
        0
    } else {
        effective_end - start_line as usize + 1
    };

    let mut structured = json!({
        "file_path": path.display().to_string(),
        "action": action,
        "start_line": start_line,
        "end_line": effective_end,
        "lines_removed": lines_changed,
        // 후속 라인 편집이 갱신된 줄 수를 기준 삼도록 편집 후 전체 줄 수를 반환한다.
        // - 전체 라인 범위 Vec 재생성 대신 memchr 개행 카운트로 O(라인) 할당 제거
        "total_lines": window_line_count(&new_text),
        "bytes": encoded.len(),
        "eol": match eol { "\r\n" => "crlf", _ => "lf" }
    });
    if let Some(label) = decoded.legacy_label() {
        structured["encoding"] = json!(label);
    }
    if dry_run {
        let headline = format!(
            "Would {action} {} (lines {start_line}-{effective_end})",
            path.display()
        );
        return preview_result(text, &new_text, headline, structured);
    }
    RawResult::structured(
        format!(
            "{action} {} (lines {start_line}-{effective_end})",
            path.display()
        ),
        structured,
    )
}
// 2. Dry-run preview ----------------------------------------------------------
// bytes: 쓰기 예정 인코딩 크기
fn preview_result(old: &str, new: &str, headline: String, mut structured: Value) -> RawResult {
    let label = structured["file_path"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    // CRLF 접힘 diff 대신 바이트 비교
    let unchanged = old == new;
    let summary = if unchanged {
        DiffSummary::default()
    } else {
        render_unified_diff(old, new, &label, &label, &DiffSpec::default())
    };
    structured["dryRun"] = json!(true);
    structured["identical"] = json!(unchanged);
    structured["insertions"] = json!(summary.insertions);
    structured["deletions"] = json!(summary.deletions);
    structured["hunks"] = json!(summary.hunks);
    let eol_only = !unchanged && summary.identical;
    if summary.eol_changed || eol_only {
        structured["eolChanged"] = json!(true);
    }

    let mut text = headline;
    text.push('\n');
    if unchanged {
        text.push_str("No changes: the edit leaves the file identical");
    } else if eol_only {
        text.push_str("Only line endings would change; the diff folds CRLF to LF");
    } else {
        text.push_str(summary.unified.trim_end_matches('\n'));
    }
    RawResult::structured(text, structured)
}
// 3. EOL and line range helpers -----------------------------------------------
fn detect_dominant_eol(text: &str) -> &'static str {
    // Count CRLF and LF in a single pass. Previously text.matches() was called twice,
    // scanning the whole string twice; this collapses it to one pass.
    let bytes = text.as_bytes();
    let mut crlf = 0usize;
    let mut lf_only = 0usize;
    for index in 0..bytes.len() {
        if bytes[index] != b'\n' {
            continue;
        }
        if index > 0 && bytes[index - 1] == b'\r' {
            crlf += 1;
        } else {
            lf_only += 1;
        }
    }
    if crlf >= lf_only && crlf > 0 {
        "\r\n"
    } else {
        "\n"
    }
}
// start_idx/end_idx(0-based, start_idx <= end_idx, 둘 다 total_lines 범위 내로 clamp된 값)
// 두 줄의 byte range만 찾는다. end_idx 줄에 닿으면 즉시 반환해 그 이후는 스캔하지 않는다.
fn line_range_pair(
    text: &str,
    start_idx: usize,
    end_idx: usize,
) -> ((usize, usize), (usize, usize)) {
    let bytes = text.as_bytes();
    let mut line_start = 0usize;
    let mut current = 0usize;
    let mut start_range = (0usize, 0usize);
    let mut end_range = (0usize, 0usize);
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let range = (line_start, index + 1);
        if current == start_idx {
            start_range = range;
        }
        if current == end_idx {
            return (start_range, range);
        }
        line_start = index + 1;
        current += 1;
    }
    if line_start < bytes.len() {
        let range = (line_start, bytes.len());
        if current == start_idx {
            start_range = range;
        }
        if current == end_idx {
            end_range = range;
        }
    }
    (start_range, end_range)
}
fn ends_with_eol(value: &str) -> bool {
    value.ends_with('\n') || value.ends_with('\r')
}
fn normalize_replacement_eol(value: &str, target_eol: &str) -> String {
    if target_eol == "\n" {
        return crlf_to_lf(value);
    }
    let lf_only = crlf_to_lf(value);
    lf_to_crlf(&lf_only)
}
fn resolve_edit_strings(text: &str, old: &str, new: &str) -> (String, String, &'static str) {
    if text.contains(old) {
        // 한 줄 앵커의 여러 줄 교체문은 파일 우세 EOL 적용
        if !old.contains('\n') && new.contains('\n') && text.contains('\n') {
            if detect_dominant_eol(text) == "\r\n" {
                let crlf_new = lf_to_crlf(new);
                if crlf_new != new {
                    return (old.to_string(), crlf_new, "lf_to_crlf");
                }
            } else if new.contains("\r\n") {
                return (old.to_string(), crlf_to_lf(new), "crlf_to_lf");
            }
        }
        return (old.to_string(), new.to_string(), "raw");
    }
    let file_has_crlf = text.contains("\r\n");
    let old_has_crlf = old.contains("\r\n");
    let new_has_crlf = new.contains("\r\n");
    if file_has_crlf && !old_has_crlf {
        let new_old = lf_to_crlf(old);
        if text.contains(&new_old) {
            let new_new = if new_has_crlf {
                new.to_string()
            } else {
                lf_to_crlf(new)
            };
            return (new_old, new_new, "lf_to_crlf");
        }
    }
    if !file_has_crlf && old_has_crlf {
        let new_old = crlf_to_lf(old);
        if text.contains(&new_old) {
            let new_new = if new_has_crlf {
                crlf_to_lf(new)
            } else {
                new.to_string()
            };
            return (new_old, new_new, "crlf_to_lf");
        }
    }
    (old.to_string(), new.to_string(), "raw")
}
fn lf_to_crlf(value: &str) -> String {
    // Drops the matches('\n').count() pre-scan and uses a heuristic capacity (12.5% headroom) for a single pass.
    let mut out = String::with_capacity(value.len() + value.len() / 8);
    let mut prev_was_cr = false;
    for ch in value.chars() {
        if ch == '\n' && !prev_was_cr {
            out.push('\r');
            out.push('\n');
        } else {
            out.push(ch);
        }
        prev_was_cr = ch == '\r';
    }
    out
}
fn crlf_to_lf(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' && chars.peek() == Some(&'\n') {
            chars.next();
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::fs::make_temp_dir;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn edit_lines_replaces_single_line_crlf_preserved() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-replace");

        let path = dir.join("sample.txt");
        let initial = (1..=20)
            .map(|index| format!("line {index}\r\n"))
            .collect::<String>();
        std::fs::write(&path, initial).unwrap();

        let result = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 15,
            "end_line": 15,
            "replacement": "line 15 REPLACED"
        }));
        assert!(!result.is_error, "{result:?}");
        let edited = std::fs::read_to_string(&path).unwrap();
        assert!(edited.contains("line 14\r\nline 15 REPLACED\r\nline 16\r\n"));
        assert!(!edited.contains("line 15\r\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_deletes_range() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-delete");

        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();

        let result = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 3,
            "end_line": 4,
            "replacement": ""
        }));
        assert!(!result.is_error, "{result:?}");
        let edited = std::fs::read_to_string(&path).unwrap();
        assert_eq!(edited, "a\nb\ne\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_inserts_after_line() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-insert");

        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();

        let result = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 2,
            "end_line": 2,
            "replacement": "INSERTED",
            "after": true
        }));
        assert!(!result.is_error, "{result:?}");
        let edited = std::fs::read_to_string(&path).unwrap();
        assert_eq!(edited, "a\nb\nINSERTED\nc\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_rejects_out_of_range() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-oor");

        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let result = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 99,
            "replacement": "X"
        }));
        assert!(result.is_error, "{result:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_rejects_empty_old_string() {
        let dir = make_temp_dir("rust-fs-mcp-edit-empty-old");

        let path = dir.join("sample.txt");
        std::fs::write(&path, "ab").unwrap();
        let result = edit_item(&json!({
            "file_path": path.display().to_string(),
            "old_string": "",
            "new_string": "x"
        }));
        assert!(result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ab");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_accepts_range_length_expected_lines() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-rangelen");

        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
        let result = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 2,
            "end_line": 4,
            "expected_lines": 3,
            "replacement": "X"
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nX\ne\n");
        // 전체 줄 수도, 범위 길이도 아니면 여전히 오류.
        let bad = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 1,
            "end_line": 1,
            "expected_lines": 99,
            "replacement": "Y"
        }));
        assert!(bad.is_error, "{bad:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_counts_multiple_replacements_single_pass() {
        let dir = make_temp_dir("rust-fs-mcp-edit-multi");
        let path = dir.join("sample.txt");
        std::fs::write(&path, "aa bb aa cc aa").unwrap();
        let result = edit_item(&json!({
            "file_path": path.display().to_string(),
            "old_string": "aa",
            "new_string": "ZZ",
            "expected_replacements": 3
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ZZ bb ZZ cc ZZ");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_without_expected_replacements_rejects_multiple_matches() {
        // 미지정 다중 매치 거부
        let dir = make_temp_dir("rust-fs-mcp-edit-default-one");
        let path = dir.join("sample.txt");
        std::fs::write(&path, "foo\nkeep\nfoo\n").unwrap();
        let result = edit_item(&json!({
            "file_path": path.display().to_string(),
            "old_string": "foo",
            "new_string": "bar"
        }));
        assert!(result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap();
        assert!(text.contains("found 2 (lines 1, 3)"), "{text}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "foo\nkeep\nfoo\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_preserves_missing_trailing_newline() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-notrail");

        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb").unwrap();
        let result = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 2,
            "end_line": 2,
            "replacement": "B"
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nB");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_replacement_path_honors_offset_length() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-replpath");

        let target = dir.join("sample.txt");
        std::fs::write(&target, "a\nb\nc\n").unwrap();
        let source = dir.join("repl.txt");
        std::fs::write(&source, "XXXBYYY").unwrap();
        let result = edit_lines_item(&json!({
            "file_path": target.display().to_string(),
            "start_line": 2,
            "end_line": 2,
            "replacement_path": source.display().to_string(),
            "replacement_offset": 3,
            "replacement_length": 1
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "a\nB\nc\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_replacement_path_no_length_not_duplicated() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-nodup");

        let target = dir.join("sample.txt");
        std::fs::write(&target, "a\nb\nc\n").unwrap();
        let source = dir.join("repl.txt");
        std::fs::write(&source, "HELLO").unwrap();
        let result = edit_lines_item(&json!({
            "file_path": target.display().to_string(),
            "start_line": 2,
            "end_line": 2,
            "replacement_path": source.display().to_string()
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "a\nHELLO\nc\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn applied_edit_responses_match_pre_dry_run_snapshot() {
        // dryRun 도입 전 응답 스냅샷
        let dir = make_temp_dir("rust-fs-mcp-edit-snapshot");
        let path = dir.join("sample.txt");
        let display = path.display().to_string();
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

        let edited = edit_item(&json!({
            "file_path": display,
            "old_string": "two",
            "new_string": "TWO"
        }));
        assert_eq!(
            edited.content[0]["text"],
            format!("Edited {display} (1 replacements)")
        );
        assert_eq!(
            edited.structured,
            Some(json!({"file_path": display, "replacements": 1, "bytes": 14, "eolMode": "raw"}))
        );
        let lines = edit_lines_item(&json!({
            "file_path": display,
            "start_line": 3,
            "replacement": "THREE"
        }));
        assert_eq!(
            lines.content[0]["text"],
            format!("replace {display} (lines 3-3)")
        );
        assert_eq!(
            lines.structured,
            Some(json!({
                "file_path": display,
                "action": "replace",
                "start_line": 3,
                "end_line": 3,
                "lines_removed": 1,
                "total_lines": 3,
                "bytes": 14,
                "eol": "lf"
            }))
        );
        let batch = handle_file_edit(&json!({"items": [{
            "file_path": display,
            "old_string": "one",
            "new_string": "ONE"
        }]}));
        assert_eq!(
            batch.content[0]["text"],
            format!(
                "file-edit: 1/1 succeeded\n\n- [1] OK {display}: Edited {display} (1 replacements)"
            )
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\nTWO\nTHREE\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn dry_run_keeps_crlf_bytes_and_diffs_only_changed_lines() {
        let dir = make_temp_dir("rust-fs-mcp-edit-dry-crlf");
        let path = dir.join("sample.txt");
        let initial = (1..=20)
            .map(|index| format!("line {index}\r\n"))
            .collect::<String>();
        std::fs::write(&path, &initial).unwrap();

        let result = edit_item(&json!({
            "file_path": path.display().to_string(),
            "old_string": "line 10\n",
            "new_string": "LINE 10\n",
            "dryRun": true
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read(&path).unwrap(), initial.as_bytes());
        let text = result.content[0]["text"].as_str().unwrap();
        assert!(text.starts_with("Would edit "), "{text}");
        assert!(text.contains("\n-line 10\n+LINE 10\n"), "{text}");
        assert!(text.contains("@@ -7,7 +7,7 @@"), "{text}");
        assert!(
            !text.contains('\r') && !text.contains(" line 1\n"),
            "{text}"
        );
        let structured = result.structured.unwrap();
        assert_eq!(structured["dryRun"], true);
        assert_eq!(structured["identical"], false);
        assert_eq!(structured["eolMode"], "lf_to_crlf");
        assert_eq!(
            (
                &structured["insertions"],
                &structured["deletions"],
                &structured["hunks"]
            ),
            (&json!(1), &json!(1), &json!(1))
        );
        assert_eq!(structured["bytes"], initial.len());
        assert!(structured.get("eolChanged").is_none());

        let lines = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 20,
            "replacement": "last",
            "dryRun": true
        }));
        assert!(!lines.is_error, "{lines:?}");
        assert_eq!(std::fs::read(&path).unwrap(), initial.as_bytes());
        let text = lines.content[0]["text"].as_str().unwrap();
        assert!(text.starts_with("Would replace "), "{text}");
        assert!(text.ends_with("-line 20\n+last"), "{text}");
        assert_eq!(lines.structured.unwrap()["total_lines"], 20);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn dry_run_keeps_euc_kr_bytes() {
        let dir = make_temp_dir("rust-fs-mcp-edit-dry-euckr");
        let path = dir.join("legacy.txt");
        let (bytes, _, _) = encoding_rs::EUC_KR.encode("한글 line1\nline2 한글\n");
        std::fs::write(&path, &bytes).unwrap();

        let result = edit_item(&json!({
            "file_path": path.display().to_string(),
            "old_string": "line2 한글",
            "new_string": "둘째 줄",
            "dryRun": true
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes.as_ref());
        let text = result.content[0]["text"].as_str().unwrap();
        assert!(text.contains("-line2 한글\n+둘째 줄"), "{text}");
        let structured = result.structured.unwrap();
        assert_eq!(structured["encoding"], "EUC-KR");
        let (expected, _, _) = encoding_rs::EUC_KR.encode("한글 line1\n둘째 줄\n");
        assert_eq!(structured["bytes"], expected.len());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn dry_run_still_reports_validation_errors() {
        let dir = make_temp_dir("rust-fs-mcp-edit-dry-errors");
        let path = dir.join("sample.txt");
        std::fs::write(&path, "foo\nkeep\nfoo\n").unwrap();
        let display = path.display().to_string();

        let ambiguous = edit_item(&json!({
            "file_path": display, "old_string": "foo", "new_string": "bar", "dryRun": true
        }));
        assert!(ambiguous.is_error, "{ambiguous:?}");
        let text = ambiguous.content[0]["text"].as_str().unwrap();
        assert!(text.contains("found 2 (lines 1, 3)"), "{text}");
        let missing = edit_item(&json!({
            "file_path": display, "old_string": "nope", "new_string": "x", "dryRun": true
        }));
        assert!(missing.is_error, "{missing:?}");
        let drift = edit_lines_item(&json!({
            "file_path": display, "start_line": 1, "expected_lines": 9,
            "replacement": "x", "dryRun": true
        }));
        assert!(drift.is_error, "{drift:?}");
        let range = edit_lines_item(&json!({
            "file_path": display, "start_line": 99, "replacement": "x", "dryRun": true
        }));
        assert!(range.is_error, "{range:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "foo\nkeep\nfoo\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn single_line_anchor_keeps_the_file_eol_in_multi_line_text() {
        let dir = make_temp_dir("rust-fs-mcp-edit-anchor-eol");
        let crlf_path = dir.join("crlf.txt");
        std::fs::write(&crlf_path, "a\r\nb\r\nc\r\n").unwrap();
        let crlf_edit = edit_item(&json!({
            "file_path": crlf_path.display().to_string(),
            "old_string": "b",
            "new_string": "x\ny"
        }));
        assert!(!crlf_edit.is_error, "{crlf_edit:?}");
        assert_eq!(
            std::fs::read_to_string(&crlf_path).unwrap(),
            "a\r\nx\r\ny\r\nc\r\n"
        );
        assert_eq!(crlf_edit.structured.unwrap()["eolMode"], "lf_to_crlf");

        let lf_path = dir.join("lf.txt");
        std::fs::write(&lf_path, "a\nb\nc\n").unwrap();
        let lf_edit = edit_item(&json!({
            "file_path": lf_path.display().to_string(),
            "old_string": "b",
            "new_string": "x\r\ny"
        }));
        assert!(!lf_edit.is_error, "{lf_edit:?}");
        assert_eq!(std::fs::read_to_string(&lf_path).unwrap(), "a\nx\ny\nc\n");
        assert_eq!(lf_edit.structured.unwrap()["eolMode"], "crlf_to_lf");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn dry_run_reports_identical_and_eol_only_results() {
        let dir = make_temp_dir("rust-fs-mcp-edit-dry-same");
        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\r\nb\r\nc\r\n").unwrap();
        let display = path.display().to_string();

        let same = edit_item(&json!({
            "file_path": display, "old_string": "b", "new_string": "b", "dryRun": true
        }));
        let text = same.content[0]["text"].as_str().unwrap();
        assert!(
            text.ends_with("\nNo changes: the edit leaves the file identical"),
            "{text}"
        );
        assert_eq!(same.structured.unwrap()["identical"], true);
        let eol = edit_item(&json!({
            "file_path": display, "old_string": "b\r\n", "new_string": "b\n", "dryRun": true
        }));
        let text = eol.content[0]["text"].as_str().unwrap();
        assert!(
            text.ends_with("\nOnly line endings would change; the diff folds CRLF to LF"),
            "{text}"
        );
        let structured = eol.structured.unwrap();
        assert_eq!(
            (&structured["identical"], &structured["eolChanged"]),
            (&json!(false), &json!(true))
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\r\nb\r\nc\r\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn dry_run_batch_keeps_diff_line_breaks() {
        let dir = make_temp_dir("rust-fs-mcp-edit-dry-batch");
        let path = dir.join("sample.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();

        let batch = handle_file_edit(&json!({"items": [{
            "file_path": path.display().to_string(),
            "old_string": "two",
            "new_string": "TWO",
            "dryRun": true
        }]}));
        let text = batch.content[0]["text"].as_str().unwrap();
        assert!(
            text.contains("\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO"),
            "{text}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        let lines = handle_file_edit_lines(&json!({"items": [{
            "file_path": path.display().to_string(),
            "start_line": 1,
            "replacement": "ONE",
            "dryRun": true
        }]}));
        let text = lines.content[0]["text"].as_str().unwrap();
        assert!(text.contains("\n-one\n+ONE\n two"), "{text}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn file_edit_matches_across_crlf_lf_mismatch() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-edit-eol-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt");
        std::fs::write(&path, "line 19\r\nline 20\r\nline 21\r\n").unwrap();

        let result = edit_item(&json!({
            "file_path": path.display().to_string(),
            "old_string": "line 20\n",
            "new_string": "",
            "expected_replacements": 1
        }));
        assert!(!result.is_error, "{result:?}");
        let edited = std::fs::read_to_string(&path).unwrap();
        assert_eq!(edited, "line 19\r\nline 21\r\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
