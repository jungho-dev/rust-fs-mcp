use rust_fs_mcp::{batch, fs_tools, response};
use serde_json::json;
use std::fs;
use std::hint::black_box;
use std::time::{Duration, Instant};

// 1. Repeatable local workloads -------------------------------------------------
#[test]
#[ignore = "local timing workload; run with --release --ignored --nocapture"]
fn measure_agent_workloads() {
    let payload = "source line with \"quotes\", \\paths and 한글\n".repeat(400);
    measure_workload("normalize-text", 2_000, || {
        response::normalize_tool_result(
            "file-read",
            response::RawResult::structured(payload.clone(), json!({ "bytes": payload.len() })),
            Duration::ZERO,
        )
    });
    let inputs = vec![
        json!({ "path": "first" }),
        json!({ "path": "second" }),
        json!({ "path": "third" }),
    ];
    measure_workload("batch-text", 1_000, || {
        let entries = batch::run_batch(&inputs, |_| response::RawResult::text(payload.clone()));
        response::normalize_tool_result(
            "file-read",
            batch::create_batch_response("file-read", entries, true),
            Duration::ZERO,
        )
    });

    let fixture = std::env::current_dir()
        .unwrap()
        .join("target")
        .join("performance-lines.txt");
    fs::create_dir_all(fixture.parent().unwrap()).unwrap();
    fs::write(
        &fixture,
        "skipped line abcdefghijklmnopqrstuvwxyz0123456789\n".repeat(100_000),
    )
    .unwrap();
    let line_args =
        json!({ "items": [{ "path": fixture, "start_line": 99_990, "line_count": 5 }] });
    measure_workload("late-line-range", 100, || {
        fs_tools::handle_file_read_line_range(&line_args)
    });
    fs::remove_file(fixture).unwrap();
}

// 2. Median timing -------------------------------------------------------------
fn measure_workload<T>(label: &str, iterations: u32, mut operation: impl FnMut() -> T) {
    for _ in 0..20 {
        black_box(operation());
    }
    let mut samples = Vec::with_capacity(7);
    for _ in 0..7 {
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(operation());
        }
        samples.push(started.elapsed().as_secs_f64() * 1_000_000.0 / f64::from(iterations));
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{label}: median_us={:.3} min_us={:.3} max_us={:.3} rounds=7 iterations={iterations}",
        samples[3], samples[0], samples[6]
    );
}
