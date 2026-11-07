//! extract.rs
//! tools::web::extract
//!
//! web-extract handler: offline HTML -> text / markdown / links / readability conversion.
//!

use super::opt_str;
use crate::core::batch::{create_batch_response, parallel_plan, run_batch, run_batch_parallel};
use crate::core::config::existing_path;
use crate::core::response::RawResult;
use crate::core::web::{parse_dump, render_html};
use serde_json::{Value, json};
use std::fs;

// 인라인 HTML 합산이 이 미만이면 web-extract는 순차 실행이 더 싸다(go extractParallelBytes).
const EXTRACT_PARALLEL_BYTES: usize = 128 * 1024;

// 1. web-extract (offline HTML conversion) ------------------------------------
pub fn handle_web_extract(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };
    // 경로 기반 아이템이 없고 인라인 HTML 합산이 작으면 병렬 디스패치 비용만 더해진다.
    let inline_only = items
        .iter()
        .all(|item| item.get("path").and_then(Value::as_str).is_none());
    let inline_bytes: usize = items
        .iter()
        .map(|item| {
            item.get("html")
                .and_then(Value::as_str)
                .map(str::len)
                .unwrap_or(0)
        })
        .sum();
    let results = if inline_only && inline_bytes < EXTRACT_PARALLEL_BYTES {
        run_batch(items, extract_one)
    } else {
        run_batch_parallel(items, parallel_plan(), extract_one)
    };
    create_batch_response("web-extract", results, true)
}

fn extract_one(item: &Value) -> RawResult {
    let dump_str = opt_str(item, "dump").unwrap_or("markdown");
    let mode = match parse_dump(dump_str) {
        Ok(mode) => mode,
        Err(error) => return RawResult::error(error),
    };
    let base = opt_str(item, "baseUrl");

    let (label, html) = if let Some(html) = opt_str(item, "html") {
        ("inline".to_string(), html.to_string())
    } else if let Some(path) = opt_str(item, "path") {
        let resolved = match existing_path(path) {
            Ok(resolved) => resolved,
            Err(error) => return RawResult::error(error),
        };
        match fs::read_to_string(&resolved) {
            Ok(text) => (resolved.display().to_string(), text),
            Err(error) => {
                return RawResult::error(format!("Failed to read {}: {error}", resolved.display()));
            }
        }
    } else {
        return RawResult::error("html or path is required");
    };

    let rendered = match render_html(mode, &html, base) {
        Ok(rendered) => rendered,
        Err(error) => return RawResult::error(error),
    };
    // chars는 rune 수(go utf8.RuneCountInString 대응). ASCII면 len으로 스캔을 생략.
    let chars = if rendered.is_ascii() {
        rendered.len()
    } else {
        rendered.chars().count()
    };
    RawResult::structured(
        format!("{label}:\n{rendered}"),
        json!({ "dump": dump_str, "chars": chars }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_extract_inline_markdown() {
        let result = handle_web_extract(&json!({
            "items": [{ "html": "<h1>Hi</h1><p>Body</p>", "dump": "markdown" }]
        }));
        assert!(!result.is_error);
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("# Hi"), "got: {text}");
    }

    #[test]
    fn web_extract_requires_html_or_path() {
        let result = handle_web_extract(&json!({ "items": [{ "dump": "text" }] }));
        // Batch of one failing item marks the whole response as error.
        assert!(result.is_error);
    }
}
