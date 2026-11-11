//! compare.rs
//! tools::fs::compare
//!
//! file-diff handler: git-free unified diff between two files or a file and inline text.
//! Both sides decode through the edit pipeline's rules (UTF-8, UTF-16 BOM, EUC-KR); binary input is refused.
//! The diff text is cut at a line boundary near the output budget while the counts stay exact.
//!

use super::shared::{OpenFailure, open_regular_file};
use crate::core::batch::{READ_PLAN, create_batch_response, run_batch_parallel};
use crate::core::config::ensure_path_allowed;
use crate::core::diff::{
    DiffAlgorithm, DiffSpec, MAX_DIFF_CONTEXT, line_ending_style, render_unified_diff,
};
use crate::core::encoding::{DecodedText, decode_bytes};
use crate::core::response::{RawResult, max_output_bytes};
use crate::tools::args::common::{bool_field, item_content};
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::path::Path;

// 측면당 입력 상한
const MAX_SIDE_BYTES: u64 = 64 * 1024 * 1024;
const INLINE_LABEL: &str = "<content>";

// 1. File diff tool -----------------------------------------------------------
pub fn handle_file_diff(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_parallel(items, READ_PLAN, file_diff_item);
    create_batch_response("file-diff", results, true)
}
fn file_diff_item(item: &Value) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };
    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };

    // 1. 비교 옵션
    let context = item
        .get("contextLines")
        .and_then(Value::as_u64)
        .map_or(3, |value| value.min(MAX_DIFF_CONTEXT as u64) as usize);
    let algorithm = match item.get("algorithm") {
        None | Some(Value::Null) => DiffAlgorithm::default(),
        Some(Value::String(name)) => match DiffAlgorithm::parse_name(name) {
            Ok(algorithm) => algorithm,
            Err(error) => return RawResult::error(error),
        },
        Some(_) => return RawResult::error("algorithm must be a string"),
    };
    let spec = DiffSpec {
        context,
        algorithm,
        ignore_whitespace: bool_field(item, "ignoreWhitespace", false),
        ..DiffSpec::default()
    };

    // 2. 우측 입력 형태
    let other_path = item.get("otherPath").and_then(Value::as_str);
    let has_content = item.get("content").is_some() || item.get("content_path").is_some();
    if other_path.is_some() && has_content {
        return RawResult::error("Give either otherPath or content/content_path, not both");
    }
    if other_path.is_none() && !has_content {
        return RawResult::error("otherPath or content/content_path is required");
    }

    // 3. 양측 읽기
    let left = match read_diff_side(&path) {
        Ok(decoded) => decoded,
        Err(error) => return RawResult::error(error),
    };
    let left_label = path.display().to_string();
    let mut right_encoding = None;
    let (right_text, right_label) = if let Some(other) = other_path {
        let other = match ensure_path_allowed(other) {
            Ok(other) => other,
            Err(error) => return RawResult::error(error),
        };
        let right = match read_diff_side(&other) {
            Ok(decoded) => decoded,
            Err(error) => return RawResult::error(error),
        };
        right_encoding = right.legacy_label();
        (right.text, other.display().to_string())
    } else {
        // 상한 초과 파일 선읽기 방지
        if let Some(content_path) = item.get("content_path").and_then(Value::as_str)
            && item.get("content_length").is_none()
        {
            let size = ensure_path_allowed(content_path)
                .ok()
                .and_then(|resolved| fs::metadata(resolved).ok())
                .map_or(0, |metadata| metadata.len());
            if size > MAX_SIDE_BYTES {
                return RawResult::error(format!(
                    "content_path is {size} bytes; file-diff compares at most {MAX_SIDE_BYTES} bytes per side"
                ));
            }
        }
        let content = match item_content(item, "content", "content_path") {
            Ok(content) => content,
            Err(error) => return RawResult::error(error),
        };
        if content.len() as u64 > MAX_SIDE_BYTES {
            return RawResult::error(format!(
                "content is {} bytes; file-diff compares at most {MAX_SIDE_BYTES} bytes per side",
                content.len()
            ));
        }
        (content, INLINE_LABEL.to_string())
    };

    // 4. diff 렌더링
    let summary = render_unified_diff(&left.text, &right_text, &left_label, &right_label, &spec);
    let mut structured = json!({
        "path": left_label,
        "identical": summary.identical,
        "insertions": summary.insertions,
        "deletions": summary.deletions,
        "hunks": summary.hunks,
        "eolChanged": summary.eol_changed
    });
    if other_path.is_some() {
        structured["otherPath"] = json!(right_label);
    }
    if let Some(label) = left.legacy_label() {
        structured["encoding"] = json!(label);
    }
    if let Some(label) = right_encoding {
        structured["otherEncoding"] = json!(label);
    }
    let left_style = line_ending_style(&left.text).unwrap_or("none");
    let right_style = line_ending_style(&right_text).unwrap_or("none");
    let eol_note = if left_style == right_style {
        format!("line endings differ: {left_style} vs {right_style} at different lines")
    } else {
        format!("line endings differ: {left_style} vs {right_style}")
    };
    if summary.identical {
        let text = if summary.eol_changed {
            format!("Files are identical ({eol_note})")
        } else {
            "Files are identical".to_string()
        };
        return RawResult::structured(text, structured);
    }

    // 5. 출력 예산 절단
    let mut unified = summary.unified;
    let budget = max_output_bytes() * 4 / 5;
    if unified.len() > budget {
        let cut = memchr::memrchr(b'\n', &unified.as_bytes()[..budget]).map_or(0, |end| end + 1);
        unified.truncate(cut);
        unified.push_str(
            "[diff truncated at the output budget; lower contextLines or diff smaller files]",
        );
        structured["truncated"] = json!(true);
    }
    let body = unified.trim_end_matches('\n');
    let text = if summary.eol_changed {
        // patch 호환 머리말 (헤더 앞)
        format!("Note: {eol_note}\n{body}")
    } else {
        body.to_string()
    };
    RawResult::structured(text, structured)
}
// 2. Side reader --------------------------------------------------------------
fn read_diff_side(path: &Path) -> Result<DecodedText, String> {
    let (mut file, metadata) = match open_regular_file(path) {
        Ok(opened) => opened,
        Err(OpenFailure::Missing) => {
            return Err(format!("Path does not exist: {}", path.display()));
        }
        Err(OpenFailure::Directory) => {
            return Err(format!("Path is a directory: {}", path.display()));
        }
        Err(OpenFailure::Unreadable(error)) => {
            return Err(format!("Failed to read {}: {error}", path.display()));
        }
    };
    if metadata.len() > MAX_SIDE_BYTES {
        return Err(format!(
            "{} is {} bytes; file-diff compares at most {MAX_SIDE_BYTES} bytes per side",
            path.display(),
            metadata.len()
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let decoded = decode_bytes(bytes);
    if decoded.lossy {
        return Err(format!(
            "{} is not valid UTF-8, UTF-16, or EUC-KR text; refusing to diff binary content",
            path.display()
        ));
    }
    Ok(decoded)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::fs::make_temp_dir;

    fn diff_text(result: &RawResult) -> &str {
        result.content[0]["text"].as_str().unwrap()
    }
    #[test]
    fn diffs_two_files_with_counts_and_labels() {
        let dir = make_temp_dir("rust-fs-mcp-diff-files");
        let left = dir.join("a.txt");
        let right = dir.join("b.txt");
        std::fs::write(&left, "a\nb\nc\n").unwrap();
        std::fs::write(&right, "a\nB\nc\nd\n").unwrap();

        let result = file_diff_item(&json!({
            "path": left.display().to_string(),
            "otherPath": right.display().to_string()
        }));
        assert!(!result.is_error, "{result:?}");
        let text = diff_text(&result);
        let header = format!("--- {}\n+++ {}\n@@ ", left.display(), right.display());
        assert!(text.starts_with(&header), "{text}");
        assert!(text.contains("\n-b\n+B\n c\n+d"), "{text}");
        let structured = result.structured.unwrap();
        assert_eq!(structured["otherPath"], right.display().to_string());
        assert_eq!(
            (
                &structured["insertions"],
                &structured["deletions"],
                &structured["hunks"],
                &structured["identical"]
            ),
            (&json!(2), &json!(1), &json!(1), &json!(false))
        );
        assert!(structured.get("encoding").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn diffs_file_against_inline_content_and_reports_identical() {
        let dir = make_temp_dir("rust-fs-mcp-diff-inline");
        let left = dir.join("a.txt");
        std::fs::write(&left, "alpha\nbeta\n").unwrap();
        let path = left.display().to_string();

        let changed = file_diff_item(&json!({ "path": path, "content": "alpha\nBETA\n" }));
        let text = diff_text(&changed);
        assert!(text.contains("\n+++ <content>\n"), "{text}");
        assert!(changed.structured.unwrap().get("otherPath").is_none());
        let same = file_diff_item(&json!({ "path": path, "content": "alpha\nbeta\n" }));
        assert_eq!(diff_text(&same), "Files are identical");
        assert_eq!(same.structured.unwrap()["identical"], true);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn flags_eol_only_and_mixed_eol_differences() {
        let dir = make_temp_dir("rust-fs-mcp-diff-eol");
        let left = dir.join("crlf.txt");
        std::fs::write(&left, "a\r\nb\r\n").unwrap();
        let path = left.display().to_string();

        let eol_only = file_diff_item(&json!({ "path": path, "content": "a\nb\n" }));
        assert_eq!(
            diff_text(&eol_only),
            "Files are identical (line endings differ: CRLF vs LF)"
        );
        assert_eq!(eol_only.structured.unwrap()["eolChanged"], true);
        let mixed = file_diff_item(&json!({ "path": path, "content": "a\nB\n" }));
        let text = diff_text(&mixed);
        assert!(
            text.starts_with("Note: line endings differ: CRLF vs LF\n--- "),
            "{text}"
        );
        assert!(!text.contains('\r'), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn reports_mixed_line_endings_as_eol_changes() {
        let dir = make_temp_dir("rust-fs-mcp-diff-mixed-eol");
        let left = dir.join("mixed.txt");
        std::fs::write(&left, "a\r\nb\n").unwrap();
        let path = left.display().to_string();

        let to_crlf = file_diff_item(&json!({ "path": path, "content": "a\r\nb\r\n" }));
        assert_eq!(
            diff_text(&to_crlf),
            "Files are identical (line endings differ: mixed vs CRLF)"
        );
        assert_eq!(to_crlf.structured.unwrap()["eolChanged"], true);
        let to_lf = file_diff_item(&json!({ "path": path, "content": "a\nb\n" }));
        assert_eq!(
            diff_text(&to_lf),
            "Files are identical (line endings differ: mixed vs LF)"
        );
        let same = file_diff_item(&json!({ "path": path, "content": "a\r\nb\n" }));
        assert_eq!(diff_text(&same), "Files are identical");
        assert_eq!(same.structured.unwrap()["eolChanged"], false);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn huge_context_lines_do_not_abort() {
        let dir = make_temp_dir("rust-fs-mcp-diff-huge-context");
        let left = dir.join("a.txt");
        std::fs::write(&left, "a\nb\nc\n").unwrap();
        let path = left.display().to_string();

        let expected = file_diff_item(&json!({ "path": path, "content": "a\nB\nc\n" }));
        for context in [u64::MAX, 1u64 << 63] {
            let result = file_diff_item(&json!({
                "path": path, "content": "a\nB\nc\n", "contextLines": context
            }));
            assert!(!result.is_error, "{result:?}");
            assert_eq!(diff_text(&result), diff_text(&expected));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn honors_whitespace_mode_and_algorithm_names() {
        let dir = make_temp_dir("rust-fs-mcp-diff-options");
        let left = dir.join("a.txt");
        std::fs::write(&left, "  x\ny\n").unwrap();
        let path = left.display().to_string();

        let exact = file_diff_item(&json!({ "path": path, "content": "    x\ny\n" }));
        assert_eq!(exact.structured.unwrap()["identical"], false);
        let loose = file_diff_item(&json!({
            "path": path, "content": "    x\ny\n", "ignoreWhitespace": true
        }));
        assert_eq!(diff_text(&loose), "Files are identical");
        let histogram = file_diff_item(&json!({
            "path": path, "content": "  x\nz\n", "algorithm": "histogram", "contextLines": 0
        }));
        assert!(!histogram.is_error, "{histogram:?}");
        assert!(diff_text(&histogram).contains("@@ -2 +2 @@\n-y\n+z"));
        let bogus = file_diff_item(&json!({ "path": path, "content": "x", "algorithm": "lcs" }));
        assert!(bogus.is_error);
        assert!(diff_text(&bogus).contains("Unknown diff algorithm 'lcs'"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn refuses_binary_sides_and_bad_right_side_shapes() {
        let dir = make_temp_dir("rust-fs-mcp-diff-refuse");
        let text_file = dir.join("a.txt");
        let binary = dir.join("blob.bin");
        std::fs::write(&text_file, "a\n").unwrap();
        std::fs::write(&binary, [0x00, 0xFF, 0x80, 0xFE, 0x01]).unwrap();
        let path = text_file.display().to_string();
        let blob = binary.display().to_string();

        let left_binary = file_diff_item(&json!({ "path": blob, "content": "a\n" }));
        assert!(left_binary.is_error);
        assert!(diff_text(&left_binary).contains("blob.bin is not valid UTF-8"));
        let right_binary = file_diff_item(&json!({ "path": path, "otherPath": blob }));
        assert!(diff_text(&right_binary).contains("blob.bin is not valid UTF-8"));
        let missing = file_diff_item(&json!({ "path": path }));
        assert_eq!(
            diff_text(&missing),
            "Error: otherPath or content/content_path is required"
        );
        let both = file_diff_item(&json!({ "path": path, "otherPath": path, "content": "a" }));
        assert!(diff_text(&both).contains("not both"));
        let directory =
            file_diff_item(&json!({ "path": dir.display().to_string(), "content": "" }));
        assert!(diff_text(&directory).starts_with("Error: Path is a directory"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn labels_legacy_encodings_per_side() {
        let dir = make_temp_dir("rust-fs-mcp-diff-encoding");
        let left = dir.join("legacy.txt");
        let right = dir.join("utf8.txt");
        let (bytes, _, _) = encoding_rs::EUC_KR.encode("한글\n");
        std::fs::write(&left, &bytes).unwrap();
        std::fs::write(&right, "한글\n").unwrap();

        let result = file_diff_item(&json!({
            "path": left.display().to_string(),
            "otherPath": right.display().to_string()
        }));
        assert_eq!(diff_text(&result), "Files are identical");
        let structured = result.structured.unwrap();
        assert_eq!(structured["encoding"], "EUC-KR");
        assert!(structured.get("otherEncoding").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn truncates_large_diff_at_a_line_boundary_with_exact_counts() {
        let dir = make_temp_dir("rust-fs-mcp-diff-budget");
        let left = dir.join("big.txt");
        let lines = max_output_bytes() / 8 + 16;
        let old: String = (0..lines).map(|index| format!("a{index}\n")).collect();
        let new: String = (0..lines).map(|index| format!("b{index}\n")).collect();
        std::fs::write(&left, &old).unwrap();

        let result = file_diff_item(&json!({ "path": left.display().to_string(), "content": new }));
        let text = diff_text(&result);
        assert!(text.len() <= max_output_bytes(), "{}", text.len());
        assert!(text.ends_with(
            "\n[diff truncated at the output budget; lower contextLines or diff smaller files]"
        ));
        let structured = result.structured.unwrap();
        assert_eq!(structured["truncated"], true);
        assert_eq!(structured["insertions"], lines);
        assert_eq!(structured["deletions"], lines);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn batch_response_keeps_diff_line_breaks() {
        let dir = make_temp_dir("rust-fs-mcp-diff-batch");
        let left = dir.join("a.txt");
        std::fs::write(&left, "one\ntwo\n").unwrap();

        let batch = handle_file_diff(&json!({"items": [
            { "path": left.display().to_string(), "content": "one\nTWO\n" }
        ]}));
        let text = batch.content[0]["text"].as_str().unwrap();
        assert!(
            text.contains("\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO"),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
