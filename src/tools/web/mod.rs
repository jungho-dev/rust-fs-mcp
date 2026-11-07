//! mod.rs
//! tools::web::mod
//!
//! Web tier handlers: web-fetch (native HTTPS), web-extract (offline HTML ->
//! text/markdown/links/readability), and download-to-file.
//! All URL paths go through the core::web SSRF boundary, body-size cap, and sandboxed writes.
//!

mod download;
mod extract;
mod fetch;

pub use download::handle_download_to_file;
pub use extract::handle_web_extract;
pub use fetch::handle_web_fetch;

use crate::core::web::FetchOptions;
use serde_json::Value;

// 1. Field readers (tolerate Claude's string-marshaled scalars) ---------------
fn fetch_options(args: &Value, default_max_bytes: u64) -> FetchOptions {
    let mut opts = FetchOptions {
        max_bytes: default_max_bytes,
        ..FetchOptions::default()
    };
    if let Some(value) = opt_u64(args, "timeoutMs") {
        opts.timeout_ms = value;
    }
    if let Some(value) = opt_u64(args, "maxBytes") {
        opts.max_bytes = value;
    }
    if let Some(value) = opt_u64(args, "maxRedirects") {
        opts.max_redirects = value.min(u32::MAX as u64) as u32;
    }
    if let Some(value) = opt_str(args, "userAgent") {
        opts.user_agent = value.to_string();
    }
    opts
}

fn opt_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn opt_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
    })
}

fn opt_bool(value: &Value, key: &str, default: bool) -> bool {
    match value.get(key) {
        Some(value) => value
            .as_bool()
            .unwrap_or_else(|| matches!(value.as_str(), Some("true"))),
        None => default,
    }
}
