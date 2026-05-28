//! verify_diff_a5e26fad.rs
//! tests::verify_diff_a5e26fad
//!
//! Manual verification binary that checks handle_git_diff returns stably for a specific repository state (a5e26fad).
//! Run directly via cargo run; prints the key fields of the normalize_tool_result output.
//!

use rust_fs_mcp::core::response::normalize_tool_result;
use rust_fs_mcp::tools::git_tools::handle_git_diff;
use serde_json::json;
use std::time::Duration;

fn main() {
    unsafe {
        std::env::set_var("RUST_FS_MCP_ALLOWED_DIRECTORIES", "C:\\git");
    }

    let args = json!({
        "path": "C:\\git\\ims",
        "source": "b98098f99222ca08ecac75ecee0249098319d216",
        "target": "a5e26fad",
        "nameOnly": true
    });
    let result = handle_git_diff(&args);
    let normalized = normalize_tool_result("git-diff", result.clone(), Duration::from_millis(0));
    let status = normalized
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    println!("status={status}");
    if result.is_error {
        println!("ERROR: {:?}", result.content);
        std::process::exit(1);
    }
    let diff = result
        .structured
        .as_ref()
        .and_then(|structured| structured.get("diff"))
        .and_then(|value| value.as_str());
    if let Some(diff) = diff {
        println!("changed files:");
        for line in diff.lines() {
            println!("  {line}");
        }
    }
}
