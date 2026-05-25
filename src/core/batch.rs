use crate::core::response::RawResult;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;

pub struct BatchItem {
    pub index: usize,
    pub input: Value,
    pub result: RawResult,
}

// 1. Run batch ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn run_batch<F>(items: Vec<Value>, mut run_item: F) -> Vec<BatchItem>
where
    F: FnMut(Value) -> RawResult,
{
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let result = run_item(item.clone());
            BatchItem {
                index: index + 1,
                input: item,
                result,
            }
        })
        .collect()
}

pub fn run_batch_parallel<F>(items: Vec<Value>, run_item: F) -> Vec<BatchItem>
where
    F: Fn(Value) -> RawResult + Sync,
{
    let total = items.len();
    if total <= 1 {
        return run_batch(items, run_item);
    }

    let workers = thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(4)
        .min(total);
    let queue = Arc::new(Mutex::new(
        items.into_iter().enumerate().collect::<VecDeque<_>>(),
    ));
    let results = Arc::new(Mutex::new(Vec::with_capacity(total)));

    thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            let run_item = &run_item;
            scope.spawn(move || {
                loop {
                    let next = queue.lock().unwrap().pop_front();
                    let Some((index, item)) = next else {
                        break;
                    };
                    let result = run_item(item.clone());
                    results.lock().unwrap().push(BatchItem {
                        index: index + 1,
                        input: item,
                        result,
                    });
                }
            });
        }
    });

    let mut results = match Arc::try_unwrap(results) {
        Ok(results) => results.into_inner().unwrap(),
        Err(results) => {
            let mut results = results.lock().unwrap();
            std::mem::take(&mut *results)
        }
    };
    results.sort_by_key(|item| item.index);
    results
}

// 2. Create batch response ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn create_batch_response(tool_name: &str, items: Vec<BatchItem>, full: bool) -> RawResult {
    let total = items.len();
    let failed = items.iter().filter(|item| item.result.is_error).count();
    let succeeded = total - failed;
    let mut lines = vec![format!(
        "{tool_name}: {succeeded}/{total} succeeded{}",
        if failed > 0 {
            format!(", {failed} failed")
        } else {
            String::new()
        }
    )];
    lines.push(String::new());
    for item in &items {
        let status = if item.result.is_error { "ERROR" } else { "OK" };
        let text = item
            .result
            .content
            .iter()
            .filter_map(|content| content.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        if full {
            lines.push(format!(
                "- [{}] {status} {}",
                item.index,
                summarize_input(&item.input)
            ));
            if !text.trim().is_empty() {
                lines.push(text);
            }
            lines.push(String::new());
        } else {
            lines.push(format!(
                "- [{}] {status} {}{}",
                item.index,
                summarize_input(&item.input),
                if text.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", text.replace('\n', " "))
                }
            ));
        }
    }
    let structured_results: Vec<Value> = items
        .iter()
        .map(|item| {
            json!({
                "index": item.index,
                "input": item.input,
                "ok": !item.result.is_error,
                "result": {
                    "content": item.result.content,
                    "structuredContent": item.result.structured,
                    "isError": item.result.is_error
                }
            })
        })
        .collect();
    let mut result = RawResult::structured(
        lines.join("\n").trim_end().to_string(),
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
    serde_json::to_string(input).unwrap_or_default()
}
