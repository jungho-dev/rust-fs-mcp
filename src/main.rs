//! main.rs
//!
//! Sole binary entry point that launches the stdio MCP server.
//! Calls protocol::server::run and exits non-zero on fatal startup errors.
//! --isolated-worker runs one fancy-regex job from stdin instead (core::isolate) and never spawns workers.
//!

use rust_fs_mcp::core::isolate;

fn main() {
    // 격리 워커 모드 (재귀 스폰 없음)
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == isolate::WORKER_FLAG)
    {
        std::process::exit(isolate::run_worker_process(
            rust_fs_mcp::search_tools::run_isolated_search,
        ));
    }
    isolate::enable_worker_spawning();
    if let Err(error) = rust_fs_mcp::server::run() {
        eprintln!("rust-fs-mcp fatal: {error}");
        std::process::exit(1);
    }
}
