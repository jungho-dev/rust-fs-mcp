//! read.rs
//! tools::fs::read
//!
//! file-read handler: text windows, legacy encodings, directories, images, binaries, and isUrl reads.
//!

use super::shared::{read_items, usize_field, window_line_count};
use crate::core::args_ref::read_text_slice;
use crate::core::batch::{READ_PLAN, create_batch_response, run_batch_parallel};
use crate::core::config::ensure_path_allowed;
use crate::core::encoding::{decode_bytes, has_utf16_bom};
use crate::core::response::RawResult;
use crate::tools::common::{bool_field, char_byte_index};
use base64::{Engine as _, engine::general_purpose};
use serde_json::{Map, Value, json};
use std::borrow::Cow;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

enum SliceRead {
    Text {
        content: String,
        line_count: usize,
        // Total file byte length, supplied by the caller's fs::metadata (read_ascii_slice
        // seeks to the window and no longer scans to EOF, so it cannot count total bytes itself).
        byte_size: u64,
    },
    NonAscii,
}
// 1. File read tool -----------------------------------------------------------
pub fn handle_file_read(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let items = read_items(args);
    if items.is_empty() {
        return RawResult::error(
            "paths or items is required (e.g. {\"paths\":[\"C:/absolute/file\"]})",
        );
    }
    let results = run_batch_parallel(&items, READ_PLAN, move |item| {
        read_item(item, allow_missing)
    });
    create_batch_response("file-read", results, true)
}
// Per-item whole-file cap. 0 is the disabled sentinel (every call site below only caps when
// this is > 0): whole-file reads return the full body regardless of size.
fn read_max_chars() -> usize {
    0
}
pub(super) fn read_item(item: &Value, allow_missing: bool) -> RawResult {
    if bool_field(item, "isUrl", false) {
        return read_url_item(item);
    }
    // 톱레벨 allowMissing 외에 아이템 단위 allowMissing 지정도 인정.
    let allow_missing = allow_missing || bool_field(item, "allowMissing", false);
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    // stat 1회로 존재·종류·크기를 함께 얻는다: 기존 exists()+is_dir()+스트리밍 게이트 metadata의
    // 3회 stat이 file-read e2e의 절반을 차지했다(이 호스트 stat 1회 ~130µs 실측).
    let Ok(metadata) = fs::metadata(&path) else {
        if allow_missing {
            return RawResult::structured(
                format!("Missing path: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    };
    if metadata.is_dir() {
        return read_directory(&path);
    }
    if let Some(mime_type) = image_mime(&path) {
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return RawResult::error(format!("Failed to read {}: {error}", path.display()));
            }
        };
        return RawResult {
            content: vec![json!({
                "type": "image",
                "data": general_purpose::STANDARD.encode(&bytes),
                "mimeType": mime_type
            })],
            structured: Some(json!({
                "path": path.display().to_string(),
                "bytes": bytes.len(),
                "mimeType": mime_type
            })),
            is_error: false,
            meta: Map::new(),
        };
    }
    let offset = usize_field(item, "offset", 0);
    let length = item
        .get("length")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    if offset > 0 || length.is_some() {
        // offset==0 은 seek(0)이 항상 안전(char/byte 위치가 같은 시작점)하므로 ASCII fast
        // path 를 먼저 시도한다. offset>0 인 byte seek 는 파일 앞부분에 non-ASCII 문자가
        // 있으면 char offset 과 byte 위치가 어긋나 엉뚱한 위치를 반환하는 correctness 버그가
        // 있어(테스트로 재현: "가나다ABCDEFGH" char offset=9 요청이 byte offset 9로 seek되어
        // 의도한 "GH" 대신 "AB"를 반환) 시도하지 않는다.
        if offset == 0 {
            match read_ascii_slice(&path, 0, length, metadata.len()) {
                Ok(SliceRead::Text {
                    content,
                    line_count,
                    byte_size,
                }) => {
                    return RawResult::structured(
                        format!("{}:\n{}", path.display(), content),
                        json!({
                            "path": path.display().to_string(),
                            "bytes": byte_size,
                            "lineCount": line_count
                        }),
                    );
                }
                Ok(SliceRead::NonAscii) => {}
                Err(error) => return RawResult::error(error),
            }
        }
        // non-ASCII 파일(위 fast path 포기 포함)과 offset>0 은 args_ref 의 증분 UTF-8
        // 스트리밍으로 처리: 전체 파일을 메모리에 올리지 않고 char offset 을 정확히 반영한다.
        // 윈도우에 NUL 이 섞이면(이진 파일) None 반환, 아래 전체 읽기 폴백의 기존 binary
        // 판정을 그대로 탄다.
        if let Some((content, line_count, byte_size)) =
            read_text_window(&path, offset, length, metadata.len())
        {
            return RawResult::structured(
                format!("{}:\n{}", path.display(), content),
                json!({
                    "path": path.display().to_string(),
                    "bytes": byte_size,
                    "lineCount": line_count,
                    "offset": offset
                }),
            );
        }
    }
    // Whole-file reads far past the cap (when read_max_chars() is raised above 0 again) stream
    // through read_ascii_slice instead of fs::read-ing the entire file: only the first max_chars
    // stay in memory while bytes/lineCount still cover the whole file. Non-ASCII files fall back
    // below. With the cap disabled this branch never triggers.
    let max_chars = read_max_chars();
    if max_chars > 0 && offset == 0 && length.is_none() && metadata.len() > (max_chars as u64) * 2 {
        match read_ascii_slice(&path, 0, Some(max_chars), metadata.len()) {
            Ok(SliceRead::Text {
                content,
                line_count,
                byte_size,
            }) => {
                // ASCII: chars == bytes, and byte_size > 2*max_chars guarantees truncation.
                let total_chars = byte_size as usize;
                let body = format!(
                    "{}:\n{}\n[truncated: returned {max_chars} of {total_chars} chars; pass offset/length to read more]",
                    path.display(),
                    content
                );
                return RawResult::structured(
                    body,
                    json!({
                        "path": path.display().to_string(),
                        "bytes": byte_size,
                        "lineCount": line_count,
                        "truncated": true,
                        "returnedChars": max_chars,
                        "totalChars": total_chars
                    }),
                );
            }
            Ok(SliceRead::NonAscii) => {}
            Err(error) => return RawResult::error(error),
        }
    }
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return RawResult::error(format!("Failed to read {}: {error}", path.display()));
        }
    };

    // UTF-16 BOM 우선 판정 (NUL 포함)
    if bytes.contains(&0) && !has_utf16_bom(&bytes) {
        return binary_result(&path, bytes);
    }
    let byte_len = bytes.len();
    // UTF-8 무복사, 그 외 재디코딩
    let decoded = decode_bytes(bytes);
    let encoding_label = decoded.legacy_label();
    let text = decoded.text;

    // Replaces the line-slice cost of text.lines().count() with a SIMD newline scan (memchr).
    let lf_count = memchr::memchr_iter(b'\n', text.as_bytes()).count();
    let line_count = if text.is_empty() {
        0
    } else if text.ends_with('\n') {
        lf_count
    } else {
        lf_count + 1
    };

    // A whole-file read (no explicit length) past read_max_chars would be capped here; disabled
    // by default (read_max_chars() == 0). An explicit length is always honored exactly as
    // requested. chars().count() <= len(), so a byte-length pre-check skips the full char scan
    // for files that cannot exceed the cap — the common case.
    let maybe_over_cap =
        length.is_none() && max_chars > 0 && text.len().saturating_sub(offset) > max_chars;
    let total_chars = if maybe_over_cap {
        text.chars().count()
    } else {
        0
    };
    let truncated = maybe_over_cap && total_chars.saturating_sub(offset) > max_chars;
    let effective_length = if truncated { Some(max_chars) } else { length };
    // whole-read(offset 0·길이 미지정·미절단)는 slice_chars 의 전체 재복사를 피하고 text 를 그대로 본문에 씀
    let sliced: Cow<'_, str> = if offset == 0 && effective_length.is_none() {
        Cow::Borrowed(&text)
    } else {
        Cow::Owned(slice_chars(&text, offset, effective_length))
    };

    let mut structured = json!({
        "path": path.display().to_string(),
        "bytes": byte_len,
        "lineCount": line_count
    });
    if offset > 0 {
        structured["offset"] = json!(offset);
    }
    if let Some(label) = encoding_label {
        structured["encoding"] = json!(label);
    }
    let body = if truncated {
        structured["truncated"] = json!(true);
        structured["returnedChars"] = json!(max_chars);
        structured["totalChars"] = json!(total_chars);
        format!(
            "{}:\n{}\n[truncated: returned {max_chars} of {total_chars} chars; pass offset/length to read more]",
            path.display(),
            sliced
        )
    } else {
        format!("{}:\n{}", path.display(), sliced)
    };

    RawResult::structured(body, structured)
}
// isUrl now routes through core::web: TLS-capable (HTTPS works), SSRF-guarded, and body-capped.
fn read_url_item(item: &Value) -> RawResult {
    let Some(url) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a URL string");
    };

    let page = match crate::core::web::http_fetch(
        url,
        &crate::core::web::FetchOptions::default(),
        false,
    ) {
        Ok(page) => page,
        Err(error) => return RawResult::error(error),
    };
    let content = page.body_text();
    let offset = usize_field(item, "offset", 0);
    let sliced = slice_chars(
        &content,
        offset,
        item.get("length")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
    );

    let mut structured = json!({
        "url": url,
        "finalUrl": page.final_url,
        "status": page.status,
        "contentType": page.content_type,
        "bytes": page.body.len()
    });
    if offset > 0 {
        structured["offset"] = json!(offset);
    }
    RawResult::structured(format!("{url}:\n{sliced}"), structured)
}
fn read_directory(path: &Path) -> RawResult {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            return RawResult::error(format!("Failed to list {}: {error}", path.display()));
        }
    };

    let mut names = Vec::new();
    for entry in entries.flatten() {
        // file_type() reuses metadata the directory scan already produced; no per-entry stat.
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        let marker = if is_dir { "/" } else { "" };
        names.push(format!("{}{}", entry.file_name().to_string_lossy(), marker));
    }
    names.sort();

    // The entry list lives in the text body once; structured carries metadata only.
    RawResult::structured(
        format!("{}:\n{}", path.display(), names.join("\n")),
        json!({
            "path": path.display().to_string(),
            "entryCount": names.len(),
            "directory": true
        }),
    )
}
// 2. Image and binary payloads ------------------------------------------------
fn image_mime(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        // svg/bmp are intentionally NOT image content: MCP image blocks reach models that
        // accept png/jpeg/gif/webp only, and an svg source must stay readable as text.
        _ => None,
    }
}
fn binary_result(path: &Path, bytes: Vec<u8>) -> RawResult {
    // Binary reads honor the same read cap as text: encode at most read_max_chars base64 chars
    // (cap_bytes is a multiple of 3, so the prefix stays valid base64 without padding).
    let total_bytes = bytes.len();
    let max_chars = read_max_chars();
    let cap_bytes = if max_chars > 0 {
        max_chars / 4 * 3
    } else {
        usize::MAX
    };
    let truncated = total_bytes > cap_bytes;
    let encoded = if truncated {
        general_purpose::STANDARD.encode(&bytes[..cap_bytes])
    } else {
        general_purpose::STANDARD.encode(&bytes)
    };
    let mut structured = json!({
        "path": path.display().to_string(),
        "binary": true,
        "bytes": total_bytes,
        "base64": encoded
    });
    if truncated {
        structured["truncated"] = json!(true);
        structured["returnedBytes"] = json!(cap_bytes);
    }
    RawResult::structured(
        format!("Binary file: {} ({total_bytes} bytes)", path.display()),
        structured,
    )
}
// 3. Window readers -----------------------------------------------------------
fn slice_chars(text: &str, offset: usize, length: Option<usize>) -> String {
    let start = char_byte_index(text, offset);
    let end = length
        .map(|length| start + char_byte_index(&text[start..], length))
        .unwrap_or(text.len());
    text[start..end].to_string()
}
// offset(char 단위)/length 윈도우를 args_ref 의 증분 UTF-8 스트리밍으로 읽는다. seek 를
// 쓰지 않아 char offset 을 항상 정확히 반영하며, 청크 단위 디코딩이라 전체 파일을
// 메모리에 올리지 않는다. 윈도우에 NUL 이 섞이거나(이진 파일) invalid UTF-8 이면 None을
// 반환해 호출부가 기존 전체 읽기 폴백(binary 판정 포함)으로 넘어가게 한다.
fn read_text_window(
    path: &Path,
    offset: usize,
    length: Option<usize>,
    file_size: u64,
) -> Option<(String, usize, u64)> {
    let content = read_text_slice(path, offset, length).ok()?;
    if content.contains('\0') {
        return None;
    }
    let line_count = window_line_count(&content);
    Some((content, line_count, file_size))
}
fn read_ascii_slice(
    path: &Path,
    offset: usize,
    length: Option<usize>,
    file_size: u64,
) -> Result<SliceRead, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    // offset 으로 seek 해 [offset, offset+length) 윈도우만 읽는다: 대형 파일 슬라이스가 앞부분을
    // 순차로 훑지 않아 O(offset+length) 가 아니라 O(length) 가 된다.
    // - byte_size 는 caller 의 stat(file_size)로 대체하므로 전체 파일 스캔이 불필요
    // - 윈도우에 null/비ASCII 가 있으면 NonAscii 를 반환해 전체 읽기 폴백(이진/char offset)에 맡김
    if offset > 0 {
        file.seek(SeekFrom::Start(offset as u64))
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    }
    let mut buffer = [0u8; 64 * 1024];
    let mut content = String::with_capacity(length.unwrap_or(0).min(1024 * 1024));
    let mut produced = 0usize;

    loop {
        // 남은 윈도우 크기만큼만 읽는다(length 미지정이면 offset 이후 EOF 까지).
        let to_read = match length {
            Some(want) => {
                let remaining = want.saturating_sub(produced);
                if remaining == 0 {
                    break;
                }
                remaining.min(buffer.len())
            }
            None => buffer.len(),
        };
        let read = file
            .read(&mut buffer[..to_read])
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        // 윈도우에 null 또는 비ASCII 가 있으면 fast 경로를 포기하고 폴백에 맡긴다.
        if chunk.iter().any(|byte| *byte == 0 || *byte >= 0x80) {
            return Ok(SliceRead::NonAscii);
        }
        // 윈도우가 순수 ASCII(전 바이트 < 0x80)이므로 UTF-8 은 항상 유효하다.
        if let Ok(text) = std::str::from_utf8(chunk) {
            content.push_str(text);
        }
        produced += read;
    }
    let line_count = window_line_count(&content);
    Ok(SliceRead::Text {
        content,
        line_count,
        byte_size: file_size,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::fs::make_temp_dir;
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn char_offset_after_leading_non_ascii_returns_correct_window() {
        // "가나다" 는 9바이트/3글자. offset=9(char)가 byte offset 로 오독되면 "가나다" 직후인
        // "AB"가 반환된다(회귀). 정확한 char offset 처리라면 9번째 문자인 "G"부터 반환된다.
        let dir = make_temp_dir("rust-fs-mcp-char-offset");
        let path = dir.join("probe.txt");
        std::fs::write(&path, "가나다ABCDEFGH").unwrap();
        let result = read_item(
            &json!({ "path": path.display().to_string(), "offset": 9, "length": 2 }),
            false,
        );
        assert!(!result.is_error, "{result:?}");
        assert!(
            result.content[0]["text"].as_str().unwrap().ends_with("GH"),
            "{result:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn non_ascii_offset_read_does_not_require_whole_file_ascii() {
        // non-ASCII 파일의 offset/length 읽기가 여전히 올바른 윈도우를 반환하는지 확인
        // (전체 fs::read 폴백이 아니라 args_ref 스트리밍 경로를 통과하는 케이스).
        let dir = make_temp_dir("rust-fs-mcp-non-ascii-window");
        let path = dir.join("probe.txt");
        std::fs::write(&path, "한글로 시작하는 파일입니다.\n둘째 줄입니다.\n").unwrap();
        let result = read_item(
            &json!({ "path": path.display().to_string(), "offset": 0, "length": 5 }),
            false,
        );
        assert!(!result.is_error, "{result:?}");
        assert!(
            result.content[0]["text"]
                .as_str()
                .unwrap()
                .ends_with("한글로 시"),
            "{result:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn offset_length_read_reports_window_line_count_not_whole_file() {
        // offset/length 페이징 읽기의 lineCount 는 반환된 윈도우 기준(전체 파일 기준 아님).
        // 파일은 5줄이지만 요청 윈도우("b\nc\n")는 2줄이어야 한다.
        let dir = make_temp_dir("rust-fs-mcp-window-line-count");
        let path = dir.join("probe.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
        let result = read_item(
            &json!({ "path": path.display().to_string(), "offset": 2, "length": 4 }),
            false,
        );
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.structured.as_ref().unwrap()["lineCount"], 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn svg_reads_as_text_not_image() {
        let dir = make_temp_dir("rust-fs-mcp-svg-text");
        let path = dir.join("icon.svg");
        std::fs::write(&path, "<svg xmlns=\"http://www.w3.org/2000/svg\"/>").unwrap();
        let result = read_item(&json!({ "path": path.display().to_string() }), false);
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content[0]["type"], "text");
        assert!(result.content[0]["text"].as_str().unwrap().contains("<svg"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn large_ascii_read_returns_full_body_without_truncation() {
        let dir = make_temp_dir("rust-fs-mcp-large-read");
        let path = dir.join("big.log");
        // 100 bytes per line * 3000 lines = 300KB; read_max_chars() is disabled (0), so a
        // whole-file read must come back untruncated regardless of size.
        let line = "x".repeat(99);
        let mut body = String::with_capacity(300_000);
        for _ in 0..3000 {
            body.push_str(&line);
            body.push('\n');
        }
        std::fs::write(&path, &body).unwrap();
        let result = read_item(&json!({ "path": path.display().to_string() }), false);
        assert!(!result.is_error, "{result:?}");
        let structured = result.structured.unwrap();
        assert_eq!(structured["bytes"], 300_000u64);
        assert_eq!(structured["lineCount"], 3000);
        assert!(structured.get("truncated").is_none());
        let text = result.content[0]["text"].as_str().unwrap();
        assert_eq!(text.matches(&line).count(), 3000);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn read_honors_item_level_allow_missing() {
        let missing = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("rust-fs-mcp-missing-probe.txt");
        let result = handle_file_read(&json!({
            "items": [{ "path": missing.display().to_string(), "allowMissing": true }]
        }));
        assert!(!result.is_error, "{result:?}");
    }
    #[test]
    fn reads_http_url() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                .unwrap();
        });

        // loopback은 SSRF guard를 통과하므로(allow_private=false) 실제 ureq 경로가 loopback
        // listener에 도달한다; core::web::http_fetch를 guard 포함해 end to end로 검증한다.
        let page = crate::core::web::http_fetch(
            &format!("http://{addr}/"),
            &crate::core::web::FetchOptions::default(),
            false,
        )
        .expect("loopback fetch");
        assert_eq!(page.status, 200);
        assert_eq!(page.body_text(), "hello");
    }
}
