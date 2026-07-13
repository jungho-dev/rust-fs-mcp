//! batch.rs
//! core::batch
//!
//! Shared batch runner that executes items[] inputs per-item.
//! Handles sequential, pooled-parallel, and mutation-safe execution plus the
//! succeededCount / failedCount / totalCount response shape.
//! Parallel batches run on a lazy persistent worker pool with an atomic work cursor;
//! the caller thread always participates, so no OS thread is spawned per request.
//!

use crate::core::config::canonical_key;
use crate::core::response::{compact_enabled, RawResult};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::fmt::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
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
pub const READ_PLAN: BatchPlan = BatchPlan { min_parallel: 3, max_workers: 8 };
pub const STAT_PLAN: BatchPlan = BatchPlan { min_parallel: 4, max_workers: 16 };
pub const SEARCH_PLAN: BatchPlan = BatchPlan { min_parallel: 2, max_workers: 2 };
pub const FETCH_PLAN: BatchPlan = BatchPlan { min_parallel: 2, max_workers: 32 };
pub const DOWNLOAD_PLAN: BatchPlan = BatchPlan { min_parallel: 2, max_workers: 16 };
const MUTATION_PLAN: BatchPlan = BatchPlan { min_parallel: 2, max_workers: 4 };
// 독립성 판정이 O(n²)이므로 병렬 mutation은 이 개수까지만 시도한다(go maxMutationFanout).
const MAX_MUTATION_FANOUT: usize = 256;

pub fn parallel_plan() -> BatchPlan {
  BatchPlan { min_parallel: 2, max_workers: available_parallelism() }
}
pub fn available_parallelism() -> usize {
  thread::available_parallelism().map(|value| value.get()).unwrap_or(4)
}
// 1. Run batch ----------------------------------------------------------------------------
// Handlers pass borrowed items; the per-item Value clone only happens in full mode for the
// input echo, so large write/edit bodies are no longer copied twice on the hot path.
pub fn run_batch<F>(items: &[Value], mut run_item: F) -> Vec<BatchItem>
where
  F: FnMut(&Value) -> RawResult,
{
  let keep_input = !compact_enabled();
  items.iter().enumerate().map(|(index, item)| BatchItem { index: index + 1, summary: summarize_input(item), input: keep_input.then(|| item.clone()), result: run_item(item) }).collect()
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
  let slots: Arc<Vec<Mutex<Option<RawResult>>>> = Arc::new((0..total).map(|_| Mutex::new(None)).collect());
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
      let result = slots[index].lock().unwrap().take().unwrap_or_else(|| RawResult::error("batch worker panicked"));
      BatchItem { index: index + 1, summary: summarize_input(input), input: keep_input.then(|| input.clone()), result }
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
  left.iter().any(|a| right.iter().any(|b| keys_conflict(a, b)))
}
// 동일 경로 또는 조상·자손 관계('/' 경계 요구)만 충돌로 본다.
fn keys_conflict(a: &str, b: &str) -> bool {
  if a == b {
    return true;
  }
  let prefix_of = |longer: &str, shorter: &str| longer.len() > shorter.len() && longer.as_bytes()[shorter.len()] == b'/' && longer.starts_with(shorter);
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
    let shared = Arc::new(PoolShared { queue: Mutex::new(VecDeque::new()), available: Condvar::new() });
    let size = available_parallelism().clamp(2, 16);
    for _ in 0..size {
      let worker_shared = Arc::clone(&shared);
      let _ = thread::Builder::new().name("fs-mcp-batch".to_string()).spawn(move || pool_worker(worker_shared));
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
  let job = Arc::new(PoolJob { total, cursor: AtomicUsize::new(0), completed: AtomicUsize::new(0), work: Box::new(work), finished: Mutex::new(false), finished_cv: Condvar::new() });
  if extra_workers > 0 {
    let shared = pool_shared();
    let mut queue = shared.queue.lock().unwrap();
    for _ in 0..extra_workers {
      queue.push_back(Arc::clone(&job));
    }
    drop(queue);
    if extra_workers == 1 {
      shared.available.notify_one();
    }
    else {
      shared.available.notify_all();
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
    }
    else {
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
      let BatchItem { index, summary: _, input, result } = item;
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
// 3. Summarize input --------------------------------------------------------------------
fn summarize_input(input: &Value) -> String {
  if let Some(path) = input.get("path").or_else(|| input.get("file_path")).and_then(Value::as_str) {
    return path.to_string();
  }
  if let (Some(source), Some(destination)) = (input.get("source").and_then(Value::as_str), input.get("destination").and_then(Value::as_str)) {
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

  #[test]
  fn parallel_batch_preserves_order() {
    let items: Vec<Value> = (0..24).map(|index| json!({ "path": format!("C:/tmp/item-{index}") })).collect();
    let results = run_batch_parallel(&items, READ_PLAN, |item| RawResult::text(item.get("path").and_then(Value::as_str).unwrap_or("").to_string()));
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
    let items: Vec<Value> = (0..2).map(|index| json!({ "path": format!("C:/tmp/{index}") })).collect();
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
    let items: Vec<Value> = (0..4).map(|index| json!({ "path": format!("C:/tmp/mut-{index}.txt") })).collect();
    let results = run_batch_mutation(&items, |item| item.get("path").and_then(Value::as_str).map(|path| vec![path.to_string()]).unwrap_or_default(), |item| RawResult::text(item.get("path").and_then(Value::as_str).unwrap_or("").to_string()));
    assert_eq!(results.len(), 4);
    for (index, item) in results.iter().enumerate() {
      assert!(item.result.content[0]["text"].as_str().unwrap().ends_with(&format!("mut-{index}.txt")));
    }
  }
}
