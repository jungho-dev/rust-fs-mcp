//! batch.rs
//! core::batch
//!
//! Shared batch runner that executes items[] inputs per-item.
//! Handles sequential, pooled-parallel, and mutation-safe execution plus the
//! succeededCount / failedCount / totalCount response shape.
//! Parallel batches run on a lazy persistent worker pool with an atomic work cursor;
//! the caller thread always participates, so no OS thread is spawned per request.
//!

use crate::core::client::current_client_host;
use crate::core::config::canonical_key;
use crate::core::response::{
    RawResult, TruncateScope, compact_enabled, effective_compress, enforce_payload_budget,
    error_text_copies, escaped_len, host_output_bytes, payload_bytes,
};
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::fmt::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;

pub struct BatchItem {
    pub index: usize,
    pub summary: String,
    // Compact responses never retain a verbatim request echo.
    pub input: Option<Value>,
    pub result: RawResult,
}
// 0. Batch plans (ported from go-fs-mcp) ---------------------------------------------------
// Below min_parallel a batch runs inline on the caller thread; at or above it, up to
// max_workers-1 pool workers join the caller on the shared cursor.
#[derive(Clone, Copy)]
pub struct BatchPlan {
    pub min_parallel: usize,
    pub max_workers: usize,
}
pub const READ_PLAN: BatchPlan = BatchPlan {
    min_parallel: 3,
    max_workers: 8,
};
pub const STAT_PLAN: BatchPlan = BatchPlan {
    min_parallel: 4,
    max_workers: 16,
};
pub const SEARCH_PLAN: BatchPlan = BatchPlan {
    min_parallel: 2,
    max_workers: 2,
};
pub const FETCH_PLAN: BatchPlan = BatchPlan {
    min_parallel: 2,
    max_workers: 32,
};
pub const DOWNLOAD_PLAN: BatchPlan = BatchPlan {
    min_parallel: 2,
    max_workers: 16,
};
const MUTATION_PLAN: BatchPlan = BatchPlan {
    min_parallel: 2,
    max_workers: 4,
};
// 독립성 판정이 O(n²)이므로 병렬 mutation은 이 개수까지만 시도한다(go maxMutationFanout).
const MAX_MUTATION_FANOUT: usize = 256;

pub fn parallel_plan() -> BatchPlan {
    BatchPlan {
        min_parallel: 2,
        max_workers: available_parallelism(),
    }
}
pub fn available_parallelism() -> usize {
    thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(4)
}
// 1. Run batch ----------------------------------------------------------------------------
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
pub fn run_batch_parallel<F>(items: &[Value], plan: BatchPlan, run_item: F) -> Vec<BatchItem>
where
    F: Fn(&Value) -> RawResult + Send + Sync + 'static,
{
    let total = items.len();
    let workers = adaptive_workers(total, plan);
    if workers <= 1 {
        return run_batch(items, run_item);
    }
    // Pool workers need 'static data: clone the (small) item objects into an Arc once.
    // Pool-eligible tools carry path-sized items, so this clone is nanoseconds versus the
    // per-request OS-thread spawns it replaces.
    let shared_items: Arc<Vec<Value>> = Arc::new(items.to_vec());
    let slots: Arc<Vec<Mutex<Option<RawResult>>>> =
        Arc::new((0..total).map(|_| Mutex::new(None)).collect());
    let job_items = Arc::clone(&shared_items);
    let job_slots = Arc::clone(&slots);
    pool_execute(total, workers - 1, move |index| {
        let result = run_item(&job_items[index]);
        *job_slots[index].lock().unwrap() = Some(result);
    });

    let keep_input = !compact_enabled();
    items
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let result = slots[index]
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| RawResult::error("batch worker panicked"));
            BatchItem {
                index: index + 1,
                summary: summarize_input(input),
                input: keep_input.then(|| input.clone()),
                result,
            }
        })
        .collect()
}
// 1a. Mutation batch ------------------------------------------------------------------------
// Filesystem mutations only run in parallel when every item touches provably independent
// paths: same path, ancestor/descendant, unknown shape, or over-fanout all fall back to the
// sequential runner, preserving the original ordering semantics (go RunBatchMutation).
pub fn run_batch_mutation<F, T>(items: &[Value], touched: T, run_item: F) -> Vec<BatchItem>
where
    F: Fn(&Value) -> RawResult + Send + Sync + 'static,
    T: Fn(&Value) -> Vec<String>,
{
    if items.len() < 2 || items.len() > MAX_MUTATION_FANOUT {
        return run_batch(items, run_item);
    }
    let mut key_sets: Vec<Vec<String>> = Vec::with_capacity(items.len());
    for item in items {
        if !item.is_object() {
            return run_batch(items, run_item);
        }
        let paths = touched(item);
        if paths.is_empty() {
            return run_batch(items, run_item);
        }
        key_sets.push(paths.iter().map(|path| canonical_key(path)).collect());
    }
    for first in 0..key_sets.len() {
        for second in (first + 1)..key_sets.len() {
            if key_sets_conflict(&key_sets[first], &key_sets[second]) {
                return run_batch(items, run_item);
            }
        }
    }
    run_batch_parallel(items, MUTATION_PLAN, run_item)
}
fn key_sets_conflict(left: &[String], right: &[String]) -> bool {
    left.iter()
        .any(|a| right.iter().any(|b| keys_conflict(a, b)))
}
// 동일 경로 또는 조상·자손 관계('/' 경계 요구)만 충돌로 본다.
fn keys_conflict(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let prefix_of = |longer: &str, shorter: &str| {
        longer.len() > shorter.len()
            && longer.as_bytes()[shorter.len()] == b'/'
            && longer.starts_with(shorter)
    };
    prefix_of(a, b) || prefix_of(b, a)
}
// 1b. Adaptive worker count -----------------------------------------------------------------
fn adaptive_workers(total: usize, plan: BatchPlan) -> usize {
    if total <= 1 {
        return 1;
    }
    if total < plan.min_parallel {
        return 1;
    }
    plan.max_workers.min(total).max(1)
}
// 1c. Persistent worker pool ------------------------------------------------------------------
// Windows OS-thread spawns cost tens of microseconds each; sub-millisecond batches were
// dominated by them. Pool threads start lazily on the first parallel batch (cold initialize
// spawns nothing) and park on a condvar between jobs. A job is fanned out by pushing extra
// Arc clones of it; a worker that pops an exhausted job drops it immediately, and the caller
// drains the cursor itself so completion never depends on pool availability.
struct PoolJob {
    total: usize,
    cursor: AtomicUsize,
    completed: AtomicUsize,
    work: Box<dyn Fn(usize) + Send + Sync>,
    finished: Mutex<bool>,
    finished_cv: Condvar,
}
impl PoolJob {
    fn run(&self) {
        loop {
            let index = self.cursor.fetch_add(1, Ordering::Relaxed);
            if index >= self.total {
                break;
            }
            // release panic=abort makes this moot; under cargo test an unwinding item must still
            // count as completed or the dispatching caller would wait forever.
            let _ = catch_unwind(AssertUnwindSafe(|| (self.work)(index)));
            if self.completed.fetch_add(1, Ordering::AcqRel) + 1 == self.total {
                *self.finished.lock().unwrap() = true;
                self.finished_cv.notify_all();
            }
        }
    }
}
struct PoolShared {
    queue: Mutex<VecDeque<Arc<PoolJob>>>,
    available: Condvar,
}
fn pool_shared() -> &'static Arc<PoolShared> {
    static POOL: OnceLock<Arc<PoolShared>> = OnceLock::new();
    POOL.get_or_init(|| {
        let shared = Arc::new(PoolShared {
            queue: Mutex::new(VecDeque::new()),
            available: Condvar::new(),
        });
        // IO 바운드 플랜(FETCH 32, DOWNLOAD 16)은 코어 수 이상의 동시성이 유효하므로 풀은
        // 코어의 2배(상한 32)까지 키운다. CPU 바운드 플랜은 각 plan.max_workers가 이미
        // 코어 수 이하로 제한하니 풀 확대의 영향을 받지 않는다(유휴 워커는 condvar 파킹).
        let size = (available_parallelism() * 2).clamp(2, 32);
        for _ in 0..size {
            let worker_shared = Arc::clone(&shared);
            let _ = thread::Builder::new()
                .name("fs-mcp-batch".to_string())
                .spawn(move || pool_worker(worker_shared));
        }
        shared
    })
}
fn pool_worker(shared: Arc<PoolShared>) {
    loop {
        let job = {
            let mut queue = shared.queue.lock().unwrap();
            loop {
                if let Some(job) = queue.pop_front() {
                    break job;
                }
                queue = shared.available.wait(queue).unwrap();
            }
        };
        job.run();
    }
}
pub(crate) fn pool_execute<W>(total: usize, extra_workers: usize, work: W)
where
    W: Fn(usize) + Send + Sync + 'static,
{
    if total == 0 {
        return;
    }
    let job = Arc::new(PoolJob {
        total,
        cursor: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
        work: Box::new(work),
        finished: Mutex::new(false),
        finished_cv: Condvar::new(),
    });
    if extra_workers > 0 {
        let shared = pool_shared();
        let mut queue = shared.queue.lock().unwrap();
        for _ in 0..extra_workers {
            queue.push_back(Arc::clone(&job));
        }
        drop(queue);
        // 잡 수만큼만 깨운다: notify_all은 소배치마다 풀 전체(최대 32 스레드)를 깨워
        // 트리비얼 3-item 배치 디스패치가 88µs였다. notify_one×N으로 10µs(실측 8.9배).
        for _ in 0..extra_workers {
            shared.available.notify_one();
        }
    }
    // Caller participates: the first index starts immediately with no wake latency, and even
    // with every pool worker busy elsewhere the job still finishes on this thread alone.
    job.run();
    let mut finished = job.finished.lock().unwrap();
    while !*finished {
        finished = job.finished_cv.wait(finished).unwrap();
    }
}
// 2. Create batch response ----------------------------------------------------------------
pub fn create_batch_response(tool_name: &str, mut items: Vec<BatchItem>, full: bool) -> RawResult {
    let total = items.len();
    let failed = items.iter().filter(|item| item.result.is_error).count();
    let succeeded = total - failed;

    // Per-item response quotas
    let host = current_client_host();
    let copies = if failed == total {
        error_text_copies(effective_compress(host))
    } else {
        1
    };
    // 배치 요약 줄·봉투 키 예약분
    let reserve = 1_024
        + items
            .iter()
            .map(|entry| escaped_len(&entry.summary) + 24)
            .sum::<usize>();
    let quotas = item_quotas(
        &items,
        host_output_bytes(host).saturating_sub(reserve),
        copies,
    );
    for (entry, quota) in items.iter_mut().zip(quotas) {
        if let Some(quota) = quota {
            enforce_payload_budget(
                &mut entry.result,
                quota,
                copies,
                TruncateScope::Item(tool_name),
            );
        }
    }
    let output_truncated = items.iter().any(|entry| {
        entry
            .result
            .meta
            .get("outputTruncated")
            .and_then(Value::as_bool)
            == Some(true)
    });
    let retained_bytes: usize = items
        .iter()
        .flat_map(|entry| &entry.result.content)
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .map(str::len)
        .sum();
    let estimated = 32 + total * 48 + retained_bytes;
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
        let has_text = item
            .result
            .content
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .any(|text| !text.trim().is_empty());
        if full {
            // 경로 헤더 중복 라벨 생략
            let first_line = item
                .result
                .content
                .iter()
                .find_map(|block| block.get("text").and_then(Value::as_str))
                .and_then(|text| text.lines().next())
                .unwrap_or("");
            if has_text && same_path_label(summary, first_line) {
                let _ = writeln!(&mut text_buf, "- [{}] {status}", item.index);
            } else {
                let _ = writeln!(&mut text_buf, "- [{}] {status} {summary}", item.index);
            }
            if has_text {
                for (block_index, text) in item
                    .result
                    .content
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .enumerate()
                {
                    if block_index > 0 {
                        text_buf.push(' ');
                    }
                    text_buf.push_str(text);
                }
                text_buf.push('\n');
            }
            text_buf.push('\n');
        } else {
            let _ = write!(&mut text_buf, "- [{}] {status} {summary}", item.index);
            if has_text {
                text_buf.push_str(": ");
                for (block_index, text) in item
                    .result
                    .content
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .enumerate()
                {
                    if block_index > 0 {
                        text_buf.push(' ');
                    }
                    for (segment_index, segment) in text.split('\n').enumerate() {
                        if segment_index > 0 {
                            text_buf.push(' ');
                        }
                        text_buf.push_str(segment);
                    }
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
                if result.meta.get("outputTruncated").and_then(Value::as_bool) == Some(true) {
                    entry.insert("truncated".to_string(), Value::Bool(true));
                }
                if let Some(data) = result.structured
                    && !data.is_null()
                {
                    entry.insert("data".to_string(), data);
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
    if output_truncated {
        result
            .meta
            .insert("outputTruncated".to_string(), Value::Bool(true));
    }
    result
}
// 2a. Water-filling quotas ----------------------------------------------------
// 소형 항목 잔여 예산 재분배
const MIN_ITEM_QUOTA: usize = 2_048;
fn item_quotas(items: &[BatchItem], budget: usize, copies: usize) -> Vec<Option<usize>> {
    let sizes: Vec<usize> = items
        .iter()
        .map(|entry| payload_bytes(&entry.result, copies))
        .collect();
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by_key(|&index| sizes[index]);
    let mut quotas = vec![None; sizes.len()];
    let mut remaining = budget;
    for (position, &index) in order.iter().enumerate() {
        let share = remaining / (order.len() - position);
        if sizes[index] <= share {
            remaining -= sizes[index];
            continue;
        }
        // 소액 몫: 봉투 절단 위임
        if share < MIN_ITEM_QUOTA {
            continue;
        }
        quotas[index] = Some(share);
        remaining -= share;
    }
    quotas
}
// 2b. Header dedupe -----------------------------------------------------------
fn same_path_label(summary: &str, first_line: &str) -> bool {
    let Some(header) = first_line.strip_suffix(':') else {
        return false;
    };
    let normalize = |text: &str| {
        text.replace('\\', "/")
            .trim_end_matches('/')
            .to_ascii_lowercase()
    };
    !summary.is_empty() && normalize(summary) == normalize(header)
}
// 3. Summarize input --------------------------------------------------------------------
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::response::max_output_bytes;

    #[test]
    fn retain_escaped_batch_item_previews() {
        let inputs: Vec<Value> = (0..3)
            .map(|index| json!({ "path": format!("marker-{index}") }))
            .collect();
        let entries = run_batch(&inputs, |input| {
            RawResult::text(format!(
                "{}: 한글\n{}",
                input["path"].as_str().unwrap(),
                "\"\\\n".repeat(2_500)
            ))
        });
        let response = create_batch_response("file-read", entries, true);
        let envelope = crate::core::response::normalize_tool_result(
            "file-read",
            response,
            std::time::Duration::ZERO,
        );
        let standard = &envelope["structuredContent"];
        let text = standard["data"]["content"][0]["text"].as_str().unwrap();
        for index in 0..3 {
            assert!(text.contains(&format!("marker-{index}: 한글")));
            assert_eq!(
                standard["data"]["structuredContent"]["results"][index]["truncated"],
                true
            );
        }
        assert!(serde_json::to_vec(standard).unwrap().len() <= max_output_bytes());
    }
    #[test]
    fn water_filling_hands_small_item_budget_to_large_items() {
        // 균등 분할 유휴 예산 회수
        let inputs: Vec<Value> = (0..5)
            .map(|index| json!({ "path": format!("item-{index}") }))
            .collect();
        let entries = run_batch(&inputs, |input| {
            let label = input["path"].as_str().unwrap();
            let size = if label.ends_with('0') || label.ends_with('1') {
                300
            } else {
                40_000
            };
            RawResult::text(format!("{label}:\n{}", "x".repeat(size)))
        });
        let response = create_batch_response("file-read", entries, true);
        let envelope = crate::core::response::normalize_tool_result(
            "file-read",
            response,
            std::time::Duration::ZERO,
        );
        let standard = serde_json::to_vec(&envelope["structuredContent"]).unwrap();
        assert!(standard.len() <= max_output_bytes(), "{}", standard.len());
        assert!(
            standard.len() * 10 >= max_output_bytes() * 8,
            "used only {} bytes",
            standard.len()
        );
    }
    #[test]
    fn header_matching_summary_is_not_repeated() {
        let inputs = vec![json!({ "path": "C:/tmp/a.txt" })];
        let entries = run_batch(&inputs, |_| RawResult::text("C:\\tmp\\a.txt:\nbody"));
        let response = create_batch_response("file-read", entries, true);
        let text = response.content[0]["text"].as_str().unwrap();
        assert!(text.contains("- [1] OK\nC:\\tmp\\a.txt:\nbody"), "{text}");
    }
    #[test]
    fn retain_partial_failure_error_context() {
        let inputs = vec![json!({ "path": "error" }), json!({ "path": "success" })];
        let entries = run_batch(&inputs, |input| {
            if input["path"] == "error" {
                RawResult::error("e".repeat(40_000))
            } else {
                RawResult::text("success marker")
            }
        });
        let response = create_batch_response("file-read", entries, true);
        assert!(!response.is_error);
        let text = response.content[0]["text"].as_str().unwrap();
        assert!(text.bytes().filter(|byte| *byte == b'e').count() > 12_000);
        assert!(text.contains("success marker"));
        assert_eq!(
            response.structured.as_ref().unwrap()["results"][0]["ok"],
            false
        );
        assert_eq!(
            response.structured.as_ref().unwrap()["results"][0]["truncated"],
            true
        );
        let envelope = crate::core::response::normalize_tool_result(
            "file-read",
            response,
            std::time::Duration::ZERO,
        );
        assert!(
            serde_json::to_vec(&envelope["structuredContent"])
                .unwrap()
                .len()
                <= max_output_bytes()
        );
    }
    #[test]
    fn parallel_batch_preserves_order() {
        let items: Vec<Value> = (0..24)
            .map(|index| json!({ "path": format!("C:/tmp/item-{index}") }))
            .collect();
        let results = run_batch_parallel(&items, READ_PLAN, |item| {
            RawResult::text(
                item.get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )
        });
        assert_eq!(results.len(), 24);
        for (index, item) in results.iter().enumerate() {
            assert_eq!(item.index, index + 1);
            let text = item.result.content[0]["text"].as_str().unwrap();
            assert!(text.ends_with(&format!("item-{index}")), "{text}");
        }
    }
    #[test]
    fn small_batch_runs_inline_below_min_parallel() {
        // min_parallel 4 (STAT_PLAN)에서 2개는 인라인 실행: 결과만 검증(스레드 여부는 관측 불가).
        let items: Vec<Value> = (0..2)
            .map(|index| json!({ "path": format!("C:/tmp/{index}") }))
            .collect();
        let results = run_batch_parallel(&items, STAT_PLAN, |_| RawResult::text("ok"));
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|item| !item.result.is_error));
    }
    #[test]
    fn mutation_conflict_keys_require_boundary() {
        assert!(keys_conflict("c:/a/b", "c:/a/b"));
        assert!(keys_conflict("c:/a", "c:/a/b"));
        assert!(keys_conflict("c:/a/b", "c:/a"));
        assert!(!keys_conflict("c:/data", "c:/database"));
        assert!(!keys_conflict("c:/a/b", "c:/a/c"));
    }
    #[test]
    fn mutation_batch_runs_independent_items() {
        let items: Vec<Value> = (0..4)
            .map(|index| json!({ "path": format!("C:/tmp/mut-{index}.txt") }))
            .collect();
        let results = run_batch_mutation(
            &items,
            |item| {
                item.get("path")
                    .and_then(Value::as_str)
                    .map(|path| vec![path.to_string()])
                    .unwrap_or_default()
            },
            |item| {
                RawResult::text(
                    item.get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                )
            },
        );
        assert_eq!(results.len(), 4);
        for (index, item) in results.iter().enumerate() {
            assert!(
                item.result.content[0]["text"]
                    .as_str()
                    .unwrap()
                    .ends_with(&format!("mut-{index}.txt"))
            );
        }
    }
    #[test]
    fn compact_summary_flattens_newlines_to_spaces() {
        // compact 요약은 1항목 1라인이므로 본문 개행이 공백으로 접혀야 함.
        let items = vec![json!({ "path": "C:/tmp/a.txt" })];
        let results = run_batch(&items, |_| RawResult::text("first\nsecond\nthird"));
        let response = create_batch_response("file-read", results, false);
        let text = response.content[0]["text"].as_str().unwrap();
        assert!(text.ends_with("C:/tmp/a.txt: first second third"), "{text}");
        assert_eq!(text.lines().count(), 3, "{text}");
    }
}
