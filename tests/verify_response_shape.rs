//! verify_response_shape.rs
//! tests::verify_response_shape
//!
//! Manual verification binary that inspects the normalized envelope shape of handle_file_read responses.
//! Checks presence and types of content / structuredContent.data / _meta.fsMcpResult / isError.
//!

use rust_fs_mcp::core::response::normalize_tool_result;
use rust_fs_mcp::tools::fs_tools::handle_file_read;
use serde_json::json;
use std::time::Duration;

fn main() {
    let args = json!({
        "paths": [
          ""
        ]
    });
    let result = handle_file_read(&args);
    let normalized = normalize_tool_result("file-read", result, Duration::from_millis(2));

    println!("=== content[0].text (display block) ===");
    let display = normalized["content"][0]["text"].as_str().unwrap_or("");
    println!("{display}");

    println!("\n=== structuredContent.durationMs ===");
    println!("{}", normalized["structuredContent"]["durationMs"]);

    let contains_reading = display.contains("Reading");
    let contains_tokens = display.contains("tokens");
    let contains_contents_chars = display.contains("contents = ");
    println!("\n=== assertions ===");
    println!("display contains 'Reading N chars'? {contains_reading}");
    println!("display contains 'tokens'?          {contains_tokens}");
    println!("display contains 'contents = '?     {contains_contents_chars}");
}
