use rust_fs_mcp::core::response::{normalize_tool_result, RawResult};
use serde_json::json;
use std::time::{Duration, Instant};

fn bench(name: &str, iters: u32, mut body: impl FnMut()) {
  let warmup = (iters / 10).max(1);
  for _ in 0..warmup {
    body();
  }
  let start = Instant::now();
  for _ in 0..iters {
    body();
  }
  let elapsed = start.elapsed();
  let per = (elapsed.as_nanos() as f64) / (iters as f64);
  println!("{name:<28} iters={iters:<9} total={elapsed:>12.3?} per_iter={per:>12.1}ns");
}
fn main() {
  // 15KB body, near the typical source file
  let body15 = "x".repeat(15_000);
  // 95KB body, near the 100K cap
  let body95 = "y".repeat(95_000);

  for _ in 0..3 {
    bench("normalize_only_15k", 50_000, || {
      let raw = RawResult::structured(body15.clone(), json!({ "path": "p", "bytes": 15000, "lineCount": 1 }));
      let _ = normalize_tool_result("file-read", raw, Duration::from_millis(0));
    });
  }
  for _ in 0..3 {
    bench("normalize_only_95k", 50_000, || {
      let raw = RawResult::structured(body95.clone(), json!({ "path": "p", "bytes": 95000, "lineCount": 1 }));
      let _ = normalize_tool_result("file-read", raw, Duration::from_millis(0));
    });
  }
  // The structured() clone itself (baseline cost we cannot remove)
  for _ in 0..3 {
    bench("just_clone_95k", 50_000, || {
      let _ = body95.clone();
    });
  }
}
