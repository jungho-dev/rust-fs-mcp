//! perf_bench.rs
//! tests::perf_bench
//!
//! Manual micro-benchmark measuring per-iter cost of ensure_path_allowed, sanitize_*, and normalize_tool_result hot paths.
//! Run the same harness twice (before and after a patch) to compare nanosecond-level changes.
//!

use rust_fs_mcp::core::config::ensure_path_allowed;
use rust_fs_mcp::core::response::{normalize_tool_result, sanitize_json, sanitize_text};
use rust_fs_mcp::tools::fs_tools::handle_file_read;
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
    let per = elapsed.as_nanos() as f64 / iters as f64;
    println!("{name:<22} iters={iters:<9} total={elapsed:>12.3?} per_iter={per:>12.1}ns");
}
fn main() {
    let project = std::env::current_dir().unwrap();
    let probe = project.join("Cargo.toml").display().to_string();
    let _ = ensure_path_allowed(&probe);

    // B2: path-resolution hot path
    bench("path_resolution", 1_000_000, || {
        let _ = ensure_path_allowed(&probe);
    });

    // B4: sanitize_text non-matching 1MB (cost of the unconditional replace allocation)
    let big_text = "x".repeat(1_000_000);
    bench("sanitize_text_1mb", 3_000, || {
        let _ = sanitize_text(&big_text);
    });

    // B4: sanitize_json 10K-element tree (clone included — for relative comparison)
    let big_value = json!({
        "results": (0..10_000)
            .map(|index| format!("line {index} content text payload"))
            .collect::<Vec<_>>(),
        "text": "sample"
    });
    bench("sanitize_json_10k", 2_000, || {
        let _ = sanitize_json(big_value.clone());
    });

    // E2E: 88KB file file-read + normalize (combines B2 and B4)
    let target = project
        .join("src")
        .join("tools")
        .join("git_tools.rs")
        .display()
        .to_string();
    bench("e2e_file_read_88k", 3_000, || {
        let result = handle_file_read(&json!({ "paths": [target.as_str()] }));
        let _ = normalize_tool_result("file-read", result, Duration::from_millis(0));
    });
}
