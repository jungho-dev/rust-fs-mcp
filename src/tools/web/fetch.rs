//! fetch.rs
//! tools::web::fetch
//!
//! web-fetch handler: native HTTPS fetch, dump rendering, char-window paging, and 4xx/5xx errors.
//!

use super::{fetch_options, opt_str, opt_u64};
use crate::core::batch::{FETCH_PLAN, create_batch_response, run_batch_parallel};
use crate::core::response::RawResult;
use crate::core::web::{DumpMode, FetchOptions, FetchedPage, http_fetch, parse_dump, render_html};
use crate::tools::common::char_byte_index;
use serde_json::{Value, json};

// 1. web-fetch (TIER-1 native HTTPS) ------------------------------------------
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
    let start = char_byte_index(&rendered, offset);
    let end = max_chars.map_or(rendered.len(), |count| {
        start + char_byte_index(&rendered[start..], count)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
