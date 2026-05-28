//! 성능 병목 패치 전후 비교용 마이크로벤치.
//! 실행: cargo run --release --example perf_bench
//! 동일 하네스를 패치 전·후에 각각 돌려 per_iter 나노초를 비교한다.

use rust_fs_mcp::core::config::{ensure_path_allowed, handle_set_config_values};
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
    handle_set_config_values(&json!({
        "items": [{
            "key": "allowedDirectories",
            "value": [project.display().to_string()]
        }]
    }));

    let probe = project.join("Cargo.toml").display().to_string();
    let _ = ensure_path_allowed(&probe);

    // B2: 경로 검증 핫패스 (current_config 복제 비용)
    bench("path_check_cached", 1_000_000, || {
        let _ = ensure_path_allowed(&probe);
    });

    // B4: sanitize_text 무매칭 1MB (replace 무조건 할당 비용)
    let big_text = "x".repeat(1_000_000);
    bench("sanitize_text_1mb", 3_000, || {
        let _ = sanitize_text(&big_text);
    });

    // B4: sanitize_json 1만 요소 트리 (clone 포함 — 상대비교용)
    let big_value = json!({
        "results": (0..10_000)
            .map(|index| format!("line {index} content text payload"))
            .collect::<Vec<_>>(),
        "text": "sample"
    });
    bench("sanitize_json_10k", 2_000, || {
        let _ = sanitize_json(big_value.clone());
    });

    // E2E: 88KB 파일 file-read + normalize (B2 + B4 복합)
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
