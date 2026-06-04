//! batch.rs
//! core::batch
//!
//! Shared batch runner that executes items[] inputs per-item.
//! Handles sequential and parallel execution plus the succeededCount / failedCount / totalCount response shape.
//!

use crate::core::response::{RawResult, compact_enabled};
use serde_json::{Map, Value, json};
use std::fmt::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

pub struct BatchItem {
    pub index: usize,
    pub summary: String,
    // Verbatim request echo, kept only for the full envelope (RUST_FS_MCP_COMPACT=0).
    pub input: Option<Value>,
    pub result: RawResult,
}

// 1. Run batch ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
// Handlers pass borrowed items; the per-item Value clone only happens in full mode for the
// input echo, so large write/edit bodies are no longer copied twice on the hot path.
pub fn run_batch<F>(items: &[Value], mut run_item: F) -> Vec<BatchItem>
where
    F: FnMut(&Value) -> RawResult,
{
    let keep_input = !compact_enabled();
    items
        .iter()
        .enumerate()
        .map(|(index, item)| BatchItem {
            index: index + 1,
            summary: summarize_input(item),
            input: keep_input.then(|| item.clone()),
            result: run_item(item),
        })
        .collect()
}

pub fn run_batch_parallel<F>(items: &[Value], run_item: F) -> Vec<BatchItem>
where
    F: Fn(&Value) -> RawResult + Sync,
{
    let total = items.len();
    if total <= 1 {
        return run_batch(items, run_item);
    }

    let workers = batch_worker_count(total);
    // Borrowed items mean the scope needs no Arc; workers only fill RawResult slots.
    let cursor = AtomicUsize::new(0);
    let raw_results = (0..total)
        .map(|_| Mutex::new(None))
        .collect::<Vec<Mutex<Option<RawResult>>>>();

    thread::scope(|scope| {
        for _ in 0..workers {
            let cursor = &cursor;
            let raw_results = &raw_results;
            let run_item = &run_item;
            scope.spawn(move || {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    if index >= items.len() {
                        break;
                    }
                    let result = run_item(&items[index]);
                    *raw_results[index].lock().unwrap() = Some(result);
                }
            });
        }
    });

    let keep_input = !compact_enabled();
    items
        .iter()
        .zip(raw_results)
        .enumerate()
        .map(|(index, (input, slot))| BatchItem {
            index: index + 1,
            summary: summarize_input(input),
            input: keep_input.then(|| input.clone()),
            result: slot.into_inner().unwrap().expect("batch worker result"),
        })
        .collect()
}

// 1a. Batch worker cap ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
// With many instances (multiple Claude Code processes) the concurrent worker count on one host
// can spike and pressure disk IOPS and thread limits, so the `RUST_FS_MCP_BATCH_WORKERS` env
// var lets callers cap the per-process worker count externally. Default behavior is unchanged.
fn batch_worker_count(total: usize) -> usize {
    let env_cap = std::env::var("RUST_FS_MCP_BATCH_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0);
    let baseline = env_cap.unwrap_or_else(|| {
        thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(4)
    });
    baseline.min(total).max(1)
}

// 2. Create batch response ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn create_batch_response(tool_name: &str, items: Vec<BatchItem>, full: bool) -> RawResult {
    let total = items.len();
    let failed = items.iter().filter(|item| item.result.is_error).count();
    let succeeded = total - failed;

    // Single pre-sized String accumulator removes the per-line format! allocation.
    let estimated = 32 + total * 48;
    let mut text_buf = String::with_capacity(estimated);
    let _ = write!(&mut text_buf, "{tool_name}: {succeeded}/{total} succeeded");
    if failed > 0 {
        let _ = write!(&mut text_buf, ", {failed} failed");
    }
    text_buf.push('\n');
    text_buf.push('\n');
    for item in &items {
        let status = if item.result.is_error { "ERROR" } else { "OK" };
        let summary = item.summary.as_str();
        // Join content directly with push_str instead of a fresh collect<Vec<&str>>+join each call.
        let mut joined = String::new();
        let mut first = true;
        for content in &item.result.content {
            if let Some(text) = content.get("text").and_then(Value::as_str) {
                if !first {
                    joined.push(' ');
                }
                joined.push_str(text);
                first = false;
            }
        }
        if full {
            let _ = writeln!(&mut text_buf, "- [{}] {status} {summary}", item.index);
            if !joined.trim().is_empty() {
                text_buf.push_str(&joined);
                text_buf.push('\n');
            }
            text_buf.push('\n');
        } else {
            let _ = write!(&mut text_buf, "- [{}] {status} {summary}", item.index);
            if !joined.trim().is_empty() {
                text_buf.push_str(": ");
                // Flatten multi-line content to a single line (`\n` becomes ' ').
                for ch in joined.chars() {
                    text_buf.push(if ch == '\n' { ' ' } else { ch });
                }
            }
            text_buf.push('\n');
        }
    }
    while text_buf.ends_with('\n') {
        text_buf.pop();
    }

    // Consume items so RawResult.content / structured can be moved out directly.
    // Compact entries carry {index, ok, data}: the request echo, the result wrapper, and the
    // ok-duplicating isError flag all re-send what the caller and batch text already hold.
    let compact = compact_enabled();
    let structured_results: Vec<Value> = items
        .into_iter()
        .map(|item| {
            let BatchItem {
                index,
                summary: _,
                input,
                result,
            } = item;
            if compact {
                let mut entry = Map::new();
                entry.insert("index".to_string(), json!(index));
                entry.insert("ok".to_string(), json!(!result.is_error));
                if let Some(data) = result.structured {
                    if !data.is_null() {
                        entry.insert("data".to_string(), data);
                    }
                }
                return Value::Object(entry);
            }
            json!({
                "index": index,
                "input": input.unwrap_or(Value::Null),
                "ok": !result.is_error,
                "result": {
                    "content": result.content,
                    "structuredContent": result.structured,
                    "isError": result.is_error
                }
            })
        })
        .collect();

    let mut result = RawResult::structured(
        text_buf,
        json!({
            "failedCount": failed,
            "results": structured_results,
            "succeededCount": succeeded,
            "toolName": tool_name,
            "totalCount": total
        }),
    );
    result.is_error = failed == total && total > 0;
    result
}

// 3. Summarize input ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
fn summarize_input(input: &Value) -> String {
    if let Some(path) = input
        .get("path")
        .or_else(|| input.get("file_path"))
        .and_then(Value::as_str)
    {
        return path.to_string();
    }
    if let (Some(source), Some(destination)) = (
        input.get("source").and_then(Value::as_str),
        input.get("destination").and_then(Value::as_str),
    ) {
        return format!("{source} -> {destination}");
    }
    if let Some(session_id) = input.get("sessionId").and_then(Value::as_str) {
        return session_id.to_string();
    }
    if let Some(pid) = input.get("pid").and_then(Value::as_i64) {
        return pid.to_string();
    }
    // Truncate at 80 chars so a large input JSON is not fully serialized as a label.
    let mut serialized = serde_json::to_string(input).unwrap_or_default();
    const MAX_SUMMARY: usize = 80;
    if serialized.chars().count() > MAX_SUMMARY {
        let truncated: String = serialized.chars().take(MAX_SUMMARY).collect();
        serialized = format!("{truncated}…");
    }
    serialized
}
