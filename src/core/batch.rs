use crate::core::response::RawResult;
use serde_json::{Value, json};
use std::fmt::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
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

    let workers = batch_worker_count(total);
    // 워커는 RawResult 만 채우고 입력 Value 는 종료 후 원본 Vec 에서 zip 으로 합친다.
    // 이로써 동일 Value 를 두 번 clone 하던 핫패스를 단일 clone 으로 줄인다.
    let items = Arc::new(items);
    let cursor = Arc::new(AtomicUsize::new(0));
    let raw_results = Arc::new(
        (0..total)
            .map(|_| Mutex::new(None))
            .collect::<Vec<Mutex<Option<RawResult>>>>(),
    );

    thread::scope(|scope| {
        for _ in 0..workers {
            let items = Arc::clone(&items);
            let cursor = Arc::clone(&cursor);
            let raw_results = Arc::clone(&raw_results);
            let run_item = &run_item;
            scope.spawn(move || {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    if index >= items.len() {
                        break;
                    }
                    let result = run_item(items[index].clone());
                    *raw_results[index].lock().unwrap() = Some(result);
                }
            });
        }
    });

    let items = Arc::try_unwrap(items)
        .unwrap_or_else(|_| unreachable!("items arc should be unique after scope"));
    let raw_results = Arc::try_unwrap(raw_results)
        .unwrap_or_else(|_| unreachable!("results arc should be unique after scope"));

    items
        .into_iter()
        .zip(raw_results)
        .enumerate()
        .map(|(index, (input, slot))| BatchItem {
            index: index + 1,
            input,
            result: slot.into_inner().unwrap().expect("batch worker result"),
        })
        .collect()
}

// 1a. Batch worker cap ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
// 멀티 인스턴스(여러 Claude Code 동시 실행) 시 한 호스트에서 동시 워커 합이 폭증해
// 디스크 IOPS·스레드 한도를 압박하므로, 환경변수 `RUST_FS_MCP_BATCH_WORKERS` 로 프로세스
// 단위 워커 상한을 외부에서 강제할 수 있게 한다. 기본값은 기존 동작과 동일하다.
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

    // 사전 capacity 잡힌 단일 String 누적기로 매 라인 format! 할당을 제거한다.
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
        let summary = summarize_input(&item.input);
        // content 합치기: 매번 collect<Vec<&str>>+join 대신 직접 push_str
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
                // 멀티라인은 한 줄로 평탄화한다 (`\n` → ' ').
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

    // items 를 consume 해 RawResult 의 content/structured 를 직접 이동시킨다.
    let structured_results: Vec<Value> = items
        .into_iter()
        .map(|item| {
            let BatchItem {
                index,
                input,
                result,
            } = item;
            json!({
                "index": index,
                "input": input,
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
    // 대형 입력 JSON 을 라벨로 전부 직렬화하지 않도록 80 자에서 자른다.
    let mut serialized = serde_json::to_string(input).unwrap_or_default();
    const MAX_SUMMARY: usize = 80;
    if serialized.chars().count() > MAX_SUMMARY {
        let truncated: String = serialized.chars().take(MAX_SUMMARY).collect();
        serialized = format!("{truncated}…");
    }
    serialized
}
