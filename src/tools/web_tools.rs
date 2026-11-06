//! web_tools.rs
//! tools::web_tools
//!
//! Web tier handlers: web-fetch (native HTTPS), web-extract (offline HTML ->
//! text/markdown/links/readability), and download-to-file.
//! All URL paths go through the core::web SSRF boundary, body-size cap, and sandboxed writes.
//!

use crate::core::batch::{
    DOWNLOAD_PLAN, FETCH_PLAN, create_batch_response, parallel_plan, run_batch, run_batch_parallel,
};
use crate::core::config::{existing_path, target_path};
use crate::core::response::RawResult;
use crate::core::web::{
    DumpMode, FetchOptions, FetchedPage, MAX_ALLOWED_BYTES, http_fetch, http_fetch_to_writer,
    parse_dump, render_html,
};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

// No default cap below the hard ceiling: an omitted maxBytes downloads up to MAX_ALLOWED_BYTES.
const DOWNLOAD_DEFAULT_MAX_BYTES: u64 = MAX_ALLOWED_BYTES;
// 인라인 HTML 합산이 이 미만이면 web-extract는 순차 실행이 더 싸다(go extractParallelBytes).
const EXTRACT_PARALLEL_BYTES: usize = 128 * 1024;

// 1. web-fetch (TIER-1 native HTTPS) --------------------------------------------------------
// 렌더 결과 페이징 창 (문자 단위)
#[derive(Clone, Copy)]
struct PageWindow {
    offset: u64,
    max_chars: Option<u64>,
}

pub fn handle_web_fetch(args: &Value) -> RawResult {
    let default_dump = opt_str(args, "dump").unwrap_or("markdown").to_string();
    let opts = fetch_options(args, FetchOptions::default().max_bytes);
    let allow_private = false;
    let window = PageWindow {
        offset: opt_u64(args, "offset").unwrap_or(0),
        max_chars: opt_u64(args, "maxChars"),
    };

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

    let results = run_batch_parallel(&items, FETCH_PLAN, move |item| {
        fetch_one(item, &opts, &default_dump, allow_private, window)
    });
    create_batch_response("web-fetch", results, true)
}

fn fetch_one(
    item: &Value,
    base_opts: &FetchOptions,
    default_dump: &str,
    allow_private: bool,
    window: PageWindow,
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
        Err(error) => {
            if page.status >= 400 {
                return RawResult::error(http_failure_text(&page, ""));
            }
            return RawResult::error(error);
        }
    };
    // 4xx/5xx 본문을 정상 텍스트로 흘리면 에이전트가 차단 페이지를 콘텐츠로 오인한다.
    // 상태·스니펫·다음 행동을 한 에러 메시지에 묶는다.
    if page.status >= 400 {
        return RawResult::error(http_failure_text(&page, &rendered));
    }

    let offset = opt_u64(item, "offset").unwrap_or(window.offset) as usize;
    let max_chars = opt_u64(item, "maxChars")
        .or(window.max_chars)
        .map(|value| value as usize);
    let total_chars = rendered.chars().count();
    let start = char_byte_offset(&rendered, offset);
    let end = max_chars.map_or(rendered.len(), |count| {
        start + char_byte_offset(&rendered[start..], count)
    });
    let page_text = &rendered[start..end];
    let returned_chars = page_text.chars().count();
    let mut body = format!("{}:\n{}", page.final_url, page_text);
    let next = offset + returned_chars;
    if max_chars.is_some() && next < total_chars {
        body.push_str(&format!(
            "\n[more: next offset={next} of {total_chars} chars]"
        ));
    }
    let mut structured = json!({
        "url": url,
        "finalUrl": page.final_url,
        "status": page.status,
        "contentType": page.content_type,
        "bytes": page.body.len(),
        "dump": dump_str,
        "totalChars": total_chars
    });
    if offset > 0 {
        structured["offset"] = json!(offset);
    }
    RawResult::structured(body, structured)
}

fn char_byte_offset(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(index, _)| index)
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

// 4xx/5xx 에러 텍스트: 상태코드 + 봇차단/인증 힌트 + 본문 스니펫(정보 손실 방지).
fn http_failure_text(page: &FetchedPage, rendered: &str) -> String {
    let hint = match page.status {
        403 | 429 | 503 => {
            "\nHint: likely a bot filter or rate limit; client-rendered or bot-gated pages are not supported."
        }
        401 => "\nHint: the resource requires authentication.",
        _ => "",
    };
    let trimmed = rendered.trim();
    let mut snippet: String = trimmed.chars().take(600).collect();
    if snippet.len() < trimmed.len() {
        snippet.push('…');
    }
    if snippet.is_empty() {
        format!("HTTP {} from {}{hint}", page.status, page.final_url)
    } else {
        format!(
            "HTTP {} from {}{hint}\nBody snippet:\n{snippet}",
            page.status, page.final_url
        )
    }
}

// 2. web-extract (offline HTML conversion) --------------------------------------------------
pub fn handle_web_extract(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };
    // 경로 기반 아이템이 없고 인라인 HTML 합산이 작으면 병렬 디스패치 비용만 더해진다.
    let inline_only = items
        .iter()
        .all(|item| item.get("path").and_then(Value::as_str).is_none());
    let inline_bytes: usize = items
        .iter()
        .map(|item| {
            item.get("html")
                .and_then(Value::as_str)
                .map(str::len)
                .unwrap_or(0)
        })
        .sum();
    let results = if inline_only && inline_bytes < EXTRACT_PARALLEL_BYTES {
        run_batch(items, extract_one)
    } else {
        run_batch_parallel(items, parallel_plan(), extract_one)
    };
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
    // chars는 rune 수(go utf8.RuneCountInString 대응). ASCII면 len으로 스캔을 생략.
    let chars = if rendered.is_ascii() {
        rendered.len()
    } else {
        rendered.chars().count()
    };
    RawResult::structured(
        format!("{label}:\n{rendered}"),
        json!({ "dump": dump_str, "chars": chars }),
    )
}

// 3. download-to-file (sandboxed URL -> file) -----------------------------------------------
pub fn handle_download_to_file(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };
    let opts = fetch_options(args, DOWNLOAD_DEFAULT_MAX_BYTES);
    let allow_private = false;
    let results = run_batch_parallel(items, DOWNLOAD_PLAN, move |item| {
        download_one(item, &opts, allow_private)
    });
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
    if let Some(parent) = target.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        return RawResult::error(format!("Failed to create parent directory: {error}"));
    }
    // 전체 본문을 메모리에 버퍼링하지 않고 임시 파일로 스트림 후 rename(go 동일: 부분
    // 다운로드가 목적지 경로에 남지 않고, 대용량도 RSS를 부풀리지 않는다).
    let temp = match create_download_temp(&target) {
        Ok(temp) => temp,
        Err(error) => return RawResult::error(error),
    };
    let meta = {
        let mut file = match fs::OpenOptions::new().write(true).open(&temp) {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_file(&temp);
                return RawResult::error(format!("Failed to open temp file: {error}"));
            }
        };
        match http_fetch_to_writer(url, &opts, allow_private, &mut file) {
            Ok(meta) => {
                if let Err(error) = file.flush() {
                    drop(file);
                    let _ = fs::remove_file(&temp);
                    return RawResult::error(format!(
                        "Failed to write {}: {error}",
                        target.display()
                    ));
                }
                meta
            }
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(&temp);
                return RawResult::error(error);
            }
        }
    };
    if overwrite
        && target.exists()
        && let Err(error) = fs::remove_file(&target)
    {
        let _ = fs::remove_file(&temp);
        return RawResult::error(format!("Failed to replace {}: {error}", target.display()));
    }
    if let Err(error) = fs::rename(&temp, &target) {
        let _ = fs::remove_file(&temp);
        return RawResult::error(format!("Failed to write {}: {error}", target.display()));
    }

    RawResult::structured(
        format!(
            "Downloaded {} bytes from {} to {}",
            meta.bytes,
            meta.final_url,
            target.display()
        ),
        json!({
            "url": url,
            "finalUrl": meta.final_url,
            "path": target.display().to_string(),
            "bytes": meta.bytes,
            "status": meta.status,
            "contentType": meta.content_type
        }),
    )
}

// 목적지와 같은 디렉터리에 충돌 없는 임시 파일을 만든다(create_new로 경합 방지).
fn create_download_temp(target: &Path) -> Result<PathBuf, String> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    for attempt in 0u32..16 {
        let candidate = dir.join(format!(".download-{stamp}-{attempt}.tmp"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Failed to create temp file: {error}")),
        }
    }
    Err("Failed to create temp file: exhausted candidates".to_string())
}

// 4. Field readers (tolerate Claude's string-marshaled scalars) -----------------------------
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
    fn web_fetch_blocks_private() {
        // loopback(localhost)은 이제 허용되지만, 메타데이터 등 사설 주소는 SSRF guard가 계속 차단한다.
        let result =
            handle_web_fetch(&json!({ "url": "http://169.254.169.254/latest/meta-data/" }));
        assert!(result.is_error);
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("169.254.169.254") || text.to_lowercase().contains("blocked"));
    }

    #[test]
    fn web_fetch_errors_on_http_4xx_with_snippet_and_hint() {
        // 403 본문이 정상 콘텐츠처럼 흐르면 안 되고, 상태·스니펫·봇 차단 힌트가
        // 담긴 에러로 끝나야 한다.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let body = "<html><body>Access denied by bot filter</body></html>";
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let result = handle_web_fetch(&json!({ "url": format!("http://{addr}/") }));
        server.join().unwrap();
        assert!(result.is_error, "{result:?}");
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(text.contains("HTTP 403"), "{text}");
        assert!(text.contains("rate limit"), "{text}");
        assert!(text.contains("Access denied"), "{text}");
    }

    #[test]
    fn web_fetch_pages_rendered_text_with_offset_and_max_chars() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let body: String = (0..1_000).map(|index| format!("{:04}\n", index)).collect();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let result = handle_web_fetch(
            &json!({ "url": format!("http://{addr}/"), "offset": 10, "maxChars": 10 }),
        );
        server.join().unwrap();
        assert!(!result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("0002\n0003\n"), "{text}");
        assert!(
            text.contains("[more: next offset=20 of 5000 chars]"),
            "{text}"
        );
    }

    #[test]
    fn download_requires_url_and_path() {
        let result =
            handle_download_to_file(&json!({ "items": [{ "url": "https://example.com/" }] }));
        assert!(result.is_error);
    }
}
