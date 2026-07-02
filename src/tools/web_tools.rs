//! web_tools.rs
//! tools::web_tools
//!
//! Web tier handlers: web-fetch (native HTTPS TIER-1), web-render (obscura headless TIER-2),
//! web-extract (offline HTML -> text/markdown/links/readability), and download-to-file.
//! All URL paths go through the core::web SSRF boundary, body-size cap, and sandboxed writes.
//!

use crate::core::batch::{create_batch_response, run_batch_parallel};
use crate::core::config::{existing_path, target_path};
use crate::core::external::{ExternalTool, run_external};
use crate::core::response::RawResult;
use crate::core::web::{
    DumpMode, FetchOptions, FetchedPage, allow_private_urls, ensure_url_allowed, http_fetch,
    parse_dump, render_html,
};
use serde_json::{Value, json};
use std::fs;

const DOWNLOAD_DEFAULT_MAX_BYTES: u64 = 50_000_000;

// 1. web-fetch (TIER-1 native HTTPS) --------------------------------------------------------
pub fn handle_web_fetch(args: &Value) -> RawResult {
    let default_dump = opt_str(args, "dump").unwrap_or("markdown");
    let opts = fetch_options(args, FetchOptions::default().max_bytes);
    let allow_private = allow_private_urls();

    let mut items: Vec<Value> = Vec::new();
    if let Some(url) = opt_str(args, "url") {
        items.push(json!({ "url": url }));
    }
    if let Some(list) = args.get("items").and_then(Value::as_array) {
        items.extend(list.iter().cloned());
    }
    if items.is_empty() {
        return RawResult::error("url or items is required");
    }

    let results = run_batch_parallel(&items, |item| {
        fetch_one(item, &opts, default_dump, allow_private)
    });
    create_batch_response("web-fetch", results, true)
}

fn fetch_one(
    item: &Value,
    base_opts: &FetchOptions,
    default_dump: &str,
    allow_private: bool,
) -> RawResult {
    let Some(url) = opt_str(item, "url") else {
        return RawResult::error("url must be a string");
    };
    let dump_str = opt_str(item, "dump").unwrap_or(default_dump);
    let mode = match parse_dump(dump_str) {
        Ok(mode) => mode,
        Err(error) => return RawResult::error(error),
    };

    let mut opts = base_opts.clone();
    if let Some(value) = opt_u64(item, "timeoutMs") {
        opts.timeout_ms = value;
    }
    if let Some(value) = opt_u64(item, "maxBytes") {
        opts.max_bytes = value;
    }
    if let Some(value) = opt_str(item, "userAgent") {
        opts.user_agent = value.to_string();
    }

    let page = match http_fetch(url, &opts, allow_private) {
        Ok(page) => page,
        Err(error) => return RawResult::error(error),
    };
    let rendered = match render_page(&page, mode) {
        Ok(rendered) => rendered,
        Err(error) => return RawResult::error(error),
    };

    RawResult::structured(
        format!("{}:\n{}", page.final_url, rendered),
        json!({
            "url": url,
            "finalUrl": page.final_url,
            "status": page.status,
            "contentType": page.content_type,
            "bytes": page.body.len(),
            "dump": dump_str
        }),
    )
}

// Non-HTML bodies (JSON, plain text) cannot be extracted, so they pass through as raw text.
fn render_page(page: &FetchedPage, mode: DumpMode) -> Result<String, String> {
    if matches!(mode, DumpMode::Html) {
        return Ok(page.body_text());
    }
    let body = page.body_text();
    if page.is_html() {
        render_html(mode, &body, Some(&page.final_url))
    } else {
        Ok(body)
    }
}

// 2. web-render (TIER-2 obscura headless browser) -------------------------------------------
pub fn handle_web_render(args: &Value) -> RawResult {
    let Some(url) = opt_str(args, "url") else {
        return RawResult::error("url must be a string");
    };
    // evalScript runs arbitrary JS in the browser and can fetch internal hosts, bypassing the IP
    // guard, so it is gated behind the same private-access opt-in as the SSRF boundary.
    if opt_str(args, "evalScript").is_some() && !allow_private_urls() {
        return RawResult::error(
            "web-render evalScript can reach internal networks and bypass the SSRF guard; set RUST_FS_MCP_ALLOW_PRIVATE_URLS=1 to enable it",
        );
    }
    if let Err(error) = ensure_url_allowed(url, allow_private_urls()) {
        return RawResult::error(error);
    }
    let dump = opt_str(args, "dump").unwrap_or("html");
    if !matches!(dump, "html" | "text" | "links") {
        return RawResult::error(format!(
            "web-render dump must be html|text|links, got '{dump}'"
        ));
    }
    let timeout_s = opt_u64(args, "timeout").unwrap_or(30);

    let mut cmd: Vec<String> = vec!["fetch".to_string(), "--dump".to_string(), dump.to_string()];
    if let Some(selector) = opt_str(args, "selector") {
        cmd.push("--selector".to_string());
        cmd.push(selector.to_string());
    }
    cmd.push("--wait".to_string());
    cmd.push(opt_u64(args, "wait").unwrap_or(5).to_string());
    cmd.push("--timeout".to_string());
    cmd.push(timeout_s.to_string());
    if let Some(wait_until) = opt_str(args, "waitUntil") {
        cmd.push("--wait-until".to_string());
        cmd.push(wait_until.to_string());
    }
    if let Some(user_agent) = opt_str(args, "userAgent") {
        cmd.push("--user-agent".to_string());
        cmd.push(user_agent.to_string());
    }
    if opt_bool(args, "stealth", false) {
        cmd.push("--stealth".to_string());
    }
    if let Some(eval_script) = opt_str(args, "evalScript") {
        cmd.push("--eval".to_string());
        cmd.push(eval_script.to_string());
    }
    if opt_bool(args, "quiet", false) {
        cmd.push("--quiet".to_string());
    }
    cmd.push(url.to_string());

    // obscura's own --timeout is in seconds; give the process a wider wall-clock ceiling.
    let wall_ms = timeout_s.saturating_add(15).saturating_mul(1000);
    let output = match run_external(ExternalTool::Obscura, &cmd, None, Some(wall_ms)) {
        Ok(output) => output,
        Err(error) => return RawResult::error(error),
    };
    if output.status_code != Some(0) {
        return RawResult::error(format!(
            "obscura fetch failed (code {:?}): {}",
            output.status_code,
            output.stderr.trim()
        ));
    }

    RawResult::structured(
        format!("{url}:\n{}", output.stdout.trim_end()),
        json!({
            "url": url,
            "dump": dump,
            "backend": output.backend,
            "exitCode": output.status_code
        }),
    )
}

// 3. web-extract (offline HTML conversion) --------------------------------------------------
pub fn handle_web_extract(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };
    let results = run_batch_parallel(items, extract_one);
    create_batch_response("web-extract", results, true)
}

fn extract_one(item: &Value) -> RawResult {
    let dump_str = opt_str(item, "dump").unwrap_or("markdown");
    let mode = match parse_dump(dump_str) {
        Ok(mode) => mode,
        Err(error) => return RawResult::error(error),
    };
    let base = opt_str(item, "baseUrl");

    let (label, html) = if let Some(html) = opt_str(item, "html") {
        ("inline".to_string(), html.to_string())
    } else if let Some(path) = opt_str(item, "path") {
        let resolved = match existing_path(path) {
            Ok(resolved) => resolved,
            Err(error) => return RawResult::error(error),
        };
        match fs::read_to_string(&resolved) {
            Ok(text) => (resolved.display().to_string(), text),
            Err(error) => {
                return RawResult::error(format!("Failed to read {}: {error}", resolved.display()));
            }
        }
    } else {
        return RawResult::error("html or path is required");
    };

    let rendered = match render_html(mode, &html, base) {
        Ok(rendered) => rendered,
        Err(error) => return RawResult::error(error),
    };
    RawResult::structured(
        format!("{label}:\n{rendered}"),
        json!({ "dump": dump_str, "chars": rendered.len() }),
    )
}

// 4. download-to-file (sandboxed URL -> file) -----------------------------------------------
pub fn handle_download_to_file(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };
    let opts = fetch_options(args, DOWNLOAD_DEFAULT_MAX_BYTES);
    let allow_private = allow_private_urls();
    let results = run_batch_parallel(items, |item| download_one(item, &opts, allow_private));
    create_batch_response("download-to-file", results, true)
}

fn download_one(item: &Value, base_opts: &FetchOptions, allow_private: bool) -> RawResult {
    let Some(url) = opt_str(item, "url") else {
        return RawResult::error("url must be a string");
    };
    let Some(path) = opt_str(item, "path") else {
        return RawResult::error("path must be a string");
    };
    let target = match target_path(path) {
        Ok(target) => target,
        Err(error) => return RawResult::error(error),
    };
    let overwrite = opt_bool(item, "overwrite", false);
    if target.exists() && !overwrite {
        return RawResult::error(format!(
            "File already exists (set overwrite:true): {}",
            target.display()
        ));
    }

    let mut opts = base_opts.clone();
    if let Some(value) = opt_u64(item, "maxBytes") {
        opts.max_bytes = value;
    }
    if let Some(value) = opt_u64(item, "timeoutMs") {
        opts.timeout_ms = value;
    }

    let page = match http_fetch(url, &opts, allow_private) {
        Ok(page) => page,
        Err(error) => return RawResult::error(error),
    };
    if let Some(parent) = target.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            return RawResult::error(format!("Failed to create parent directory: {error}"));
        }
    }
    if let Err(error) = fs::write(&target, &page.body) {
        return RawResult::error(format!("Failed to write {}: {error}", target.display()));
    }

    RawResult::structured(
        format!(
            "Downloaded {} bytes from {} to {}",
            page.body.len(),
            page.final_url,
            target.display()
        ),
        json!({
            "url": url,
            "finalUrl": page.final_url,
            "path": target.display().to_string(),
            "bytes": page.body.len(),
            "status": page.status,
            "contentType": page.content_type
        }),
    )
}

// 5. Field readers (tolerate Claude's string-marshaled scalars) -----------------------------
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_extract_inline_markdown() {
        let result = handle_web_extract(&json!({
            "items": [{ "html": "<h1>Hi</h1><p>Body</p>", "dump": "markdown" }]
        }));
        assert!(!result.is_error);
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("# Hi"), "got: {text}");
    }

    #[test]
    fn web_extract_requires_html_or_path() {
        let result = handle_web_extract(&json!({ "items": [{ "dump": "text" }] }));
        // Batch of one failing item marks the whole response as error.
        assert!(result.is_error);
    }

    #[test]
    fn web_fetch_blocks_loopback() {
        let result = handle_web_fetch(&json!({ "url": "http://127.0.0.1/" }));
        assert!(result.is_error);
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("127.0.0.1") || text.to_lowercase().contains("blocked"));
    }

    #[test]
    fn web_render_blocks_private_before_spawn() {
        // SSRF guard runs before obscura is spawned, so this never touches the binary.
        let result = handle_web_render(&json!({ "url": "http://10.0.0.1/" }));
        assert!(result.is_error);
    }

    #[test]
    fn web_render_rejects_eval_without_allow_private() {
        // evalScript is gated before the URL guard, so no DNS/browser is touched here.
        let result = handle_web_render(
            &json!({ "url": "http://example.invalid/", "evalScript": "return 1" }),
        );
        assert!(result.is_error);
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("evalScript"), "got: {text}");
    }

    #[test]
    fn download_requires_url_and_path() {
        let result =
            handle_download_to_file(&json!({ "items": [{ "url": "https://example.com/" }] }));
        assert!(result.is_error);
    }
}
