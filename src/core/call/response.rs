//! response.rs
//! core::call::response
//!
//! Normalizes a RawResult into the public MCP envelope (content / structuredContent / _meta / isError).
//! Output is bounded to max_output_bytes() so a result stays under the client MCP output-token cap;
//! applies text and JSON sanitization plus duration measurement on the same path.
//! compress = "lossless" (config.toml) sends the body as one plain text block without structuredContent.
//! An unset compress and the output budget fall back to the detected MCP host policy (core::client).
//!

use crate::core::client::{ClientHost, current_client_host};
use crate::core::settings::{CompressMode, settings};
use serde_json::{Map, Value, json};
use std::env;
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct RawResult {
    pub content: Vec<Value>,
    pub structured: Option<Value>,
    pub is_error: bool,
    pub meta: Map<String, Value>,
}
impl RawResult {
    // 1. Text result ----------------------------------------------------------------------------
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![text_content(text.into())],
            structured: None,
            is_error: false,
            meta: Map::new(),
        }
    }
    // 2. Structured result ---------------------------------------------------------------------
    pub fn structured(text: impl Into<String>, structured: Value) -> Self {
        Self {
            content: vec![text_content(text.into())],
            structured: Some(sanitize_json(structured)),
            is_error: false,
            meta: Map::new(),
        }
    }
    // 3. Error result --------------------------------------------------------------------------
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![text_content(format!("Error: {}", message.into()))],
            structured: None,
            is_error: true,
            meta: Map::new(),
        }
    }
}
// 4. Text content --------------------------------------------------------------------------
pub fn text_content(text: String) -> Value {
    json!({
      "type": "text",
      "text": sanitize_owned(text)
    })
}
// 5. Normalize tool result -----------------------------------------------------------------
pub fn compact_enabled() -> bool {
    true
}
pub fn normalize_tool_result(tool_name: &str, mut result: RawResult, duration: Duration) -> Value {
    let host = current_client_host();
    let mode = effective_compress(host);
    enforce_output_budget(tool_name, &mut result, host_output_bytes(host), mode);
    match mode {
        CompressMode::Off => build_envelope(tool_name, result, duration),
        CompressMode::Lossless => build_text_envelope(tool_name, result, duration),
    }
}
fn build_envelope(tool_name: &str, result: RawResult, duration: Duration) -> Value {
    let is_error = result.is_error;
    let content = normalize_content(result.content);
    let status = if is_error { "error" } else { "success" };
    let duration_ms = duration.as_millis() as u64;
    let compact = true;
    // The combined text feeds only error messages and the full-mode data.text copy, so the
    // compact success path skips re-joining (and re-allocating) the whole body.
    let text = if is_error || !compact {
        combined_text(&content)
    } else {
        String::new()
    };
    let structured = result.structured.unwrap_or(Value::Null);
    let has_structured = !structured.is_null();
    let error = if is_error {
        json!({ "message": text })
    } else {
        Value::Null
    };
    let error_message = if is_error {
        Value::String(text.clone())
    } else {
        Value::Null
    };
    let mut data = json!({
      "content": content,
      "structuredContent": structured
    });
    if !compact {
        data["text"] = Value::String(text.clone());
    }
    // Compact keeps {data, durationMs} (+error only on failure); error:null, schemaVersion,
    // status, and toolName re-state what the caller and isError already convey on every call.
    let standard = if compact {
        let mut map = Map::new();
        map.insert("data".to_string(), data);
        map.insert("durationMs".to_string(), json!(duration_ms));
        if is_error {
            map.insert("error".to_string(), error);
        }
        Value::Object(map)
    } else {
        json!({
          "data": data,
          "durationMs": duration_ms,
          "error": error,
          "schemaVersion": 1,
          "status": status,
          "toolName": tool_name
        })
    };
    let output_truncated = result
        .meta
        .get("outputTruncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let display = create_display_text(tool_name, status, &standard, duration_ms, output_truncated);
    // 오류 사유 동봉 (클라이언트 노출용)
    let display = if is_error {
        format!("{display}\n{text}")
    } else {
        display
    };
    let mut fs_meta = Map::new();
    fs_meta.insert("contentTypes".to_string(), json!(["text"]));
    fs_meta.insert("durationMs".to_string(), json!(duration_ms));
    fs_meta.insert("errorMessage".to_string(), error_message);
    fs_meta.insert(
        "hasStructuredContent".to_string(),
        Value::Bool(has_structured),
    );
    fs_meta.insert("schemaVersion".to_string(), json!(1));
    fs_meta.insert("status".to_string(), json!(status));
    fs_meta.insert("toolName".to_string(), json!(tool_name));
    fs_meta.extend(
        result
            .meta
            .into_iter()
            .map(|(key, value)| (key, sanitize_json(value))),
    );
    let meta = json!({ "fsMcpResult": fs_meta });
    let mut out = Map::new();
    out.insert("content".to_string(), json!([text_content(display)]));
    out.insert("structuredContent".to_string(), standard);
    out.insert("_meta".to_string(), meta);
    if is_error {
        out.insert("isError".to_string(), Value::Bool(true));
    }
    Value::Object(out)
}
// 5a. Output budget ---------------------------------------------------------------------------
// Re-enabled: results are pre-truncated so a success comes back inline instead of overflowing the
// client MCP output-token cap and being dumped to a file the agent then cannot search. The client
// cap is Claude Code's 25,000-token MAX_MCP_OUTPUT_TOKENS. Token density varies by content: ASCII
// JSON runs ~2.4 bytes/token, but markdown/URL-heavy web bodies run ~1.4 bytes/token, so a
// 34,932-byte web-fetch result was rejected while the old 52,000-byte budget (calibrated on ASCII)
// let it through. 30,000 bytes stays under 25,000 tokens even at the observed ~1.4 bytes/token.
pub(crate) const MAX_STANDARD_BYTES: usize = 30_000;
// Guard: keep the budget below the smallest result size observed to breach the client cap
// (a 34,932-byte web-fetch result), leaving headroom for the display text and _meta wrapper.
const _: () = assert!(MAX_STANDARD_BYTES < 34_000);
// 패스마다 축소 보장 여유분
const TRUNCATION_SLACK_BYTES: usize = 256;
// 절단 안내문 예약분
const NOTICE_RESERVE_BYTES: usize = 400;
// 줄 경계 스냅 범위
const LINE_SNAP_BYTES: usize = 2_048;
// Serialized-size allowance for the fixed envelope keys wrapping content/structuredContent.
const ENVELOPE_OVERHEAD_BYTES: usize = 512;
// 배치 항목 래퍼 크기 ({index, ok, truncated})
const ITEM_OVERHEAD_BYTES: usize = 64;
// 오류 본문 복제 횟수
const ERROR_TEXT_COPIES: usize = 4;
const MIN_OUTPUT_BYTES: usize = 4_096;
const MAX_OUTPUT_BYTES: usize = 1_000_000;
// 레거시 바이트 env, 토큰 상한 env
static OUTPUT_ENV: OnceLock<(Option<String>, Option<String>)> = OnceLock::new();

// 5b. Budget configuration ----------------------------------------------------
// config.toml > 레거시 환경 변수 > 토큰 상한 × 1.2 > 호스트 기본값
pub fn max_output_bytes() -> usize {
    host_output_bytes(current_client_host())
}
// 호스트별 예산 (카탈로그는 ClaudeCode 고정)
pub fn host_output_bytes(host: ClientHost) -> usize {
    let (legacy, tokens) = OUTPUT_ENV.get_or_init(|| {
        (
            env::var("RUST_FS_MCP_OUTPUT_BYTES").ok(),
            env::var("MAX_MCP_OUTPUT_TOKENS").ok(),
        )
    });
    resolve_output_budget(
        settings().max_output_bytes,
        legacy.as_deref(),
        tokens.as_deref(),
        host.default_output_bytes(),
    )
}
fn resolve_output_budget(
    configured: Option<usize>,
    legacy: Option<&str>,
    tokens: Option<&str>,
    host_default: Option<usize>,
) -> usize {
    let legacy = legacy.and_then(|text| text.trim().parse::<usize>().ok());
    let scaled = tokens
        .and_then(|text| text.trim().parse::<usize>().ok())
        .map(|count| count.saturating_mul(6) / 5);
    configured
        .or(legacy)
        .or(scaled)
        .or(host_default)
        .map_or(MAX_STANDARD_BYTES, |bytes| {
            bytes.clamp(MIN_OUTPUT_BYTES, MAX_OUTPUT_BYTES)
        })
}
// 5c. Truncation scope --------------------------------------------------------
// 항목 단위만 이어 읽기 커서 산출
#[derive(Clone, Copy)]
pub(crate) enum TruncateScope<'a> {
    Envelope(&'a str),
    Item(&'a str),
}
const TOOL_HINTS: &[(&str, &str)] = &[
    ("file-read", "pass offset/length to read the rest"),
    (
        "file-read-line-range",
        "continue with a later start_line or a smaller line_count",
    ),
    (
        "fs-search",
        "narrow with filePattern, a deeper path, or a lower maxResults",
    ),
    (
        "git-diff",
        "scope with paths[], or start with stat:true or nameOnly:true",
    ),
    ("git-show", "use stat:true, filePath, or fewer objects"),
    ("git-status", "pass includeUntracked:false"),
    (
        "dir-list",
        "lower depth, set maxEntries, or list a subdirectory",
    ),
    (
        "web-fetch",
        "pass offset/maxChars to page the rendered text",
    ),
    ("web-extract", "use dump:readability or fewer items"),
];
fn truncation_hint(tool: &str) -> &'static str {
    TOOL_HINTS.iter().find(|(name, _)| *name == tool).map_or(
        "request smaller slices or fewer batch items",
        |(_, hint)| hint,
    )
}

fn enforce_output_budget(
    tool_name: &str,
    result: &mut RawResult,
    budget: usize,
    mode: CompressMode,
) {
    let copies = if result.is_error {
        error_text_copies(mode)
    } else {
        1
    };
    enforce_payload_budget(result, budget, copies, TruncateScope::Envelope(tool_name));
}
// 오류 본문 복제 수 (off envelope만 4회)
pub(crate) fn error_text_copies(mode: CompressMode) -> usize {
    match mode {
        CompressMode::Off => ERROR_TEXT_COPIES,
        CompressMode::Lossless => 1,
    }
}
// 호스트별 compress (config.toml 명시값 우선)
pub(crate) fn effective_compress(host: ClientHost) -> CompressMode {
    settings().compress.unwrap_or(host.default_compress())
}
// 예산 비교용 직렬화 크기
pub(crate) fn payload_bytes(result: &RawResult, copies: usize) -> usize {
    let structured_bytes = result.structured.as_ref().map(serialized_len).unwrap_or(4);
    let content_bytes: usize = result.content.iter().map(content_item_bytes).sum();
    content_bytes * copies + structured_bytes + ITEM_OVERHEAD_BYTES
}
// Shared envelope budget
pub(crate) fn enforce_payload_budget(
    result: &mut RawResult,
    budget: usize,
    copies: usize,
    scope: TruncateScope<'_>,
) {
    // 버짓 비활성(usize::MAX)이면 측정용 직렬화 자체가 낭비이므로 즉시 반환한다.
    // 활성 시에도 아래 측정이 매 호출 전체 페이로드를 1회 직렬화한다는 점에 유의.
    if budget == usize::MAX {
        return;
    }
    let content_bound: usize = result
        .content
        .iter()
        .map(|block| {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                estimate_json_bytes(block) + 1
            } else {
                64
            }
        })
        .sum();
    let structured_bound = result
        .structured
        .as_ref()
        .map(estimate_json_bytes)
        .unwrap_or(4);
    let overhead = match scope {
        TruncateScope::Envelope(_) => ENVELOPE_OVERHEAD_BYTES,
        TruncateScope::Item(_) => ITEM_OVERHEAD_BYTES,
    };
    if content_bound
        .saturating_mul(copies)
        .saturating_add(structured_bound)
        .saturating_add(overhead)
        <= budget
    {
        return;
    }
    let mut truncated = false;
    for _ in 0..8 {
        // serde_json renders a missing structured payload as `null` (4 bytes).
        let structured_bytes = result.structured.as_ref().map(serialized_len).unwrap_or(4);
        let content_bytes: usize = result.content.iter().map(content_item_bytes).sum();
        let total = content_bytes * copies + structured_bytes + overhead;
        if total <= budget {
            break;
        }
        let overflow = total - budget;
        if truncate_largest_text(
            &mut result.content,
            overflow.div_ceil(copies) + TRUNCATION_SLACK_BYTES,
            scope,
            result.structured.as_ref(),
        ) {
            truncated = true;
            continue;
        }
        if drop_structured_tail(result.structured.as_mut(), overflow) {
            truncated = true;
            continue;
        }
        if stub_structured(&mut result.structured) {
            truncated = true;
            continue;
        }
        break;
    }
    if truncated {
        result
            .meta
            .insert("outputTruncated".to_string(), Value::Bool(true));
    }
}
// 바이트 수만 세는 io::Write sink
// - to_string() 콘텐츠 크기만큼의 매 호출 할당 제거
// - 예산 초과 시 최대 8패스까지 반복 호출되어 콜당 절감 누적
struct ByteCounter(usize);
impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn serialized_len(value: &Value) -> usize {
    let mut counter = ByteCounter(0);
    let _ = serde_json::to_writer(&mut counter, value);
    counter.0
}
// Conservative JSON size bound
fn estimate_json_bytes(payload: &Value) -> usize {
    match payload {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(_) => 64,
        Value::String(text) => text.len().saturating_mul(6).saturating_add(2),
        Value::Array(entries) => entries.iter().fold(2usize, |bytes, entry| {
            bytes
                .saturating_add(estimate_json_bytes(entry))
                .saturating_add(1)
        }),
        Value::Object(fields) => fields.iter().fold(2usize, |bytes, (key, field)| {
            bytes
                .saturating_add(key.len().saturating_mul(6))
                .saturating_add(4)
                .saturating_add(estimate_json_bytes(field))
        }),
    }
}
// Non-text blocks (images) are exempt: clients meter them separately and a cut base64 body
// would be corrupt rather than shorter, so they only count a fixed wrapper allowance.
fn content_item_bytes(item: &Value) -> usize {
    if item.get("type").and_then(Value::as_str) == Some("text") {
        serialized_len(item) + 1
    } else {
        64
    }
}
// 이스케이프 기준 절단 (0바이트 회귀 방지)
fn truncate_largest_text(
    content: &mut [Value],
    cut_bytes: usize,
    scope: TruncateScope<'_>,
    structured: Option<&Value>,
) -> bool {
    let mut largest: Option<(usize, usize)> = None;
    for (index, item) in content.iter().enumerate() {
        if item.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let length = item
            .get("text")
            .and_then(Value::as_str)
            .map(str::len)
            .unwrap_or(0);
        // Bodies at or below the notice reserve are not worth cutting; this also keeps an
        // already-appended notice from being re-truncated on later passes.
        if length > NOTICE_RESERVE_BYTES && largest.is_none_or(|(_, best)| length > best) {
            largest = Some((index, length));
        }
    }
    let Some((index, length)) = largest else {
        return false;
    };
    let Some(text) = content[index].get("text").and_then(Value::as_str) else {
        return false;
    };
    let target = escaped_len(text)
        .saturating_sub(cut_bytes)
        .saturating_sub(NOTICE_RESERVE_BYTES);
    let mut keep = escaped_prefix_end(text, target);
    if let Some(newline) = text[..keep].rfind('\n')
        && keep - newline <= LINE_SNAP_BYTES
    {
        keep = newline + 1;
    }
    let kept = &text[..keep];
    let (tool, cursor) = match scope {
        TruncateScope::Envelope(tool) => (tool, None),
        TruncateScope::Item(tool) => (tool, continuation_cursor(tool, kept, structured)),
    };
    let cursor = cursor
        .map(|next| format!("next: {next}; "))
        .unwrap_or_default();
    let notice = format!(
        "\n[truncated: kept {keep} of {length} body bytes to fit the client output limit; {cursor}{}]",
        truncation_hint(tool)
    );
    let mut body = String::with_capacity(keep + notice.len());
    body.push_str(kept);
    body.push_str(&notice);
    content[index]["text"] = Value::String(body);
    true
}
// 항목 이어 읽기 커서
fn continuation_cursor(tool: &str, kept: &str, structured: Option<&Value>) -> Option<String> {
    match tool {
        "file-read" | "web-fetch" => {
            // 경로·URL 헤더 제외
            let header_end = kept.find('\n')? + 1;
            let base = structured
                .and_then(|value| value.get("offset"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let chars = kept[header_end..].chars().count() as u64;
            Some(format!("offset={}", base + chars))
        }
        "file-read-line-range" => {
            let whole = kept.ends_with('\n');
            let mut lines = kept.trim_end_matches('\n').rsplit('\n');
            let line_number = |line: &str| {
                line.split_once(": ")
                    .and_then(|(number, _)| number.parse::<u64>().ok())
            };
            match line_number(lines.next()?) {
                Some(number) if whole => Some(format!("start_line={}", number + 1)),
                // 잘린 줄은 같은 줄부터 다시 읽도록 안내
                Some(number) => Some(format!(
                    "start_line={number} (line {number} was cut; read it with file-read offset/length)"
                )),
                None => Some(format!("start_line={}", line_number(lines.next()?)? + 1)),
            }
        }
        _ => None,
    }
}
// JSON 바이트별 이스케이프 폭
fn escaped_byte_width(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
        0x00..=0x1f => 6,
        _ => 1,
    }
}
pub(crate) fn escaped_len(text: &str) -> usize {
    text.bytes().map(escaped_byte_width).sum()
}
// 예산 내 최장 접두부
fn escaped_prefix_end(text: &str, budget: usize) -> usize {
    let mut used = 0usize;
    for (index, byte) in text.bytes().enumerate() {
        used += escaped_byte_width(byte);
        if used > budget {
            return floor_char_boundary(text, index);
        }
    }
    text.len()
}
// Batch structured entries drop from the tail once the text body alone cannot shrink the
// payload enough; resultsDropped records how many entries were shed.
fn drop_structured_tail(structured: Option<&mut Value>, overflow: usize) -> bool {
    let Some(root) = structured else {
        return false;
    };
    let Some(results) = root.get_mut("results").and_then(Value::as_array_mut) else {
        return false;
    };
    if results.len() <= 1 {
        return false;
    }
    let mut shed = 0usize;
    let mut dropped = 0u64;
    while results.len() > 1 && shed < overflow + TRUNCATION_SLACK_BYTES {
        let Some(entry) = results.pop() else {
            break;
        };
        shed += serialized_len(&entry) + 1;
        dropped += 1;
    }
    if dropped == 0 {
        return false;
    }
    let already = root
        .get("resultsDropped")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    root["resultsDropped"] = json!(already + dropped);
    true
}
// Last resort for a single oversized structured payload without a results[] tail: replace it
// with a stub so the response never breaches the client ceiling.
fn stub_structured(structured: &mut Option<Value>) -> bool {
    let Some(root) = structured else {
        return false;
    };
    if root.get("structuredTruncated").is_some() {
        return false;
    }
    let original = serialized_len(root);
    if original <= ENVELOPE_OVERHEAD_BYTES {
        return false;
    }
    *structured = Some(json!({ "structuredTruncated": true, "originalBytes": original }));
    true
}
// str::floor_char_boundary is still unstable; clamp the cut index down to a char boundary.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut boundary = index;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}
// 5d. Lossless envelope -------------------------------------------------------
// Claude Code forwards structuredContent instead of content when both exist, so this mode
// drops structuredContent: the body reaches the model as plain text without JSON escaping,
// and only structured fields the text does not already carry follow as one `[meta]` line.
const META_NOISE_KEYS: &[&str] = &[
    "backend",
    "index",
    "ok",
    "toolName",
    "succeededCount",
    "failedCount",
];
// 본문 중복 시 생략 대상
const META_ECHO_KEYS: &[&str] = &["path", "url", "source", "destination"];
fn build_text_envelope(tool_name: &str, result: RawResult, duration: Duration) -> Value {
    let is_error = result.is_error;
    let duration_ms = duration.as_millis() as u64;
    let status = if is_error { "error" } else { "success" };
    let mut text = combined_text(&result.content);
    let meta_line = result
        .structured
        .as_ref()
        .and_then(|structured| summarize_structured(structured, &text));
    if let Some(meta_line) = meta_line {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[meta] ");
        text.push_str(&meta_line);
    }
    let mut content: Vec<Value> = result
        .content
        .into_iter()
        .filter(|block| block.get("type").and_then(Value::as_str) != Some("text"))
        .collect();
    if !text.is_empty() || content.is_empty() {
        content.insert(0, text_content(text));
    }
    let mut fs_meta = Map::new();
    fs_meta.insert(
        "compress".to_string(),
        json!(CompressMode::Lossless.as_str()),
    );
    fs_meta.insert("durationMs".to_string(), json!(duration_ms));
    fs_meta.insert("status".to_string(), json!(status));
    fs_meta.insert("toolName".to_string(), json!(tool_name));
    fs_meta.extend(
        result
            .meta
            .into_iter()
            .map(|(key, value)| (key, sanitize_json(value))),
    );
    let mut out = Map::new();
    out.insert("content".to_string(), Value::Array(content));
    out.insert("_meta".to_string(), json!({ "fsMcpResult": fs_meta }));
    if is_error {
        out.insert("isError".to_string(), Value::Bool(true));
    }
    Value::Object(out)
}
// Compact one-line JSON of the structured fields the text lacks, or None when nothing is left.
fn summarize_structured(structured: &Value, text: &str) -> Option<String> {
    let mut pruned = prune_meta_value(structured, text)?;
    // 배치 상위 건수 (헤더 중복)
    if structured.get("results").is_some_and(Value::is_array)
        && let Some(root) = pruned.as_object_mut()
    {
        root.remove("totalCount");
        if root.is_empty() {
            return None;
        }
    }
    serde_json::to_string(&pruned).ok()
}
fn prune_meta_value(node: &Value, text: &str) -> Option<Value> {
    match node {
        Value::Null => None,
        Value::Object(fields) => {
            let mut kept = Map::new();
            for (key, child) in fields {
                if META_NOISE_KEYS.contains(&key.as_str()) {
                    continue;
                }
                if META_ECHO_KEYS.contains(&key.as_str())
                    && child.as_str().is_some_and(|echo| text.contains(echo))
                {
                    continue;
                }
                if let Some(pruned) = prune_meta_value(child, text) {
                    kept.insert(key.clone(), pruned);
                }
            }
            // 배치 항목 data 래퍼 해제
            if kept.len() == 1
                && let Some(inner) = kept.remove("data")
            {
                return Some(inner);
            }
            if kept.is_empty() {
                None
            } else {
                Some(Value::Object(kept))
            }
        }
        // 항목 위치 보존 (빈 항목 null)
        Value::Array(entries) => {
            let pruned: Vec<Value> = entries
                .iter()
                .map(|entry| prune_meta_value(entry, text).unwrap_or(Value::Null))
                .collect();
            if pruned.iter().all(Value::is_null) {
                None
            } else {
                Some(Value::Array(pruned))
            }
        }
        scalar => Some(scalar.clone()),
    }
}
// 6. Normalize content -----------------------------------------------------------------------
// sanitizer가 identity로 무력화되어 있으므로(9절 참조) 재래핑은 순수 clone 낭비다.
// content 항목을 그대로 통과시켜 본문 전체 복사를 호출마다 1회 제거한다.
fn normalize_content(content: Vec<Value>) -> Vec<Value> {
    if content.is_empty() {
        return vec![text_content(String::new())];
    }
    content
}
// 7. Combined text --------------------------------------------------------------------------
// Accumulate directly into a pre-sized String without an intermediate `Vec<&str>` allocation.
fn combined_text(content: &[Value]) -> String {
    let mut total_len = 0usize;
    let mut first = true;
    for item in content {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            if !first {
                total_len += 1;
            }
            total_len += text.len();
            first = false;
        }
    }
    let mut out = String::with_capacity(total_len);
    let mut first = true;
    for item in content {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            if !first {
                out.push('\n');
            }
            out.push_str(text);
            first = false;
        }
    }
    out
}
// 8. Display text --------------------------------------------------------------------------
fn create_display_text(
    tool_name: &str,
    status: &str,
    standard: &Value,
    duration_ms: u64,
    truncated: bool,
) -> String {
    let items = standard["data"]["structuredContent"]["totalCount"]
        .as_u64()
        .or_else(|| {
            standard["data"]["structuredContent"]["results"]
                .as_array()
                .map(|items| items.len() as u64)
        })
        .unwrap_or(1);
    let duration = duration_ms as f64 / 1000.0;
    let truncated_line = if truncated {
        "• truncated = true\n"
    } else {
        ""
    };

    format!(
        "-----------------------------------\n\
          • tool = {tool_name}\n\
          • items = {items}\n\
          • status = {status}\n\
          {truncated_line}\
          • duration = {duration:.3} sec\n\
          -----------------------------------"
    )
}
// 9. Sanitize text -------------------------------------------------------------------------
// go-fs-mcp parity: the end-token sanitizer is neutered upstream (end and safe tokens are
// identical), so both sanitizers pass values through. This removes the per-response contains
// scan over large bodies and the full recursive rebuild of every structured Value — one of
// the largest per-call allocation sources. The functions stay as the single seam to
// re-enable sanitization later.
pub fn sanitize_text(value: &str) -> String {
    value.to_string()
}
fn sanitize_owned(text: String) -> String {
    text
}
// 10. Sanitize JSON ------------------------------------------------------------------------
pub fn sanitize_json(value: Value) -> Value {
    value
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaped_sizes_match_serde_serialization() {
        // 전 ASCII 제어 문자와 다바이트 문자
        let mut samples: Vec<String> = (0u8..0x80)
            .map(|byte| format!("a{}z", char::from(byte)))
            .collect();
        samples.push("\"quoted\" \\path\\ 한글 \u{7f} \u{80} \u{1F600}\r\n\t".repeat(50));
        samples.push(String::new());
        for text in &samples {
            let serialized = serialized_len(&Value::String(text.clone())) - 2;
            assert_eq!(escaped_len(text), serialized, "{text:?}");
            let per_byte: usize = text.bytes().map(escaped_byte_width).sum();
            assert_eq!(escaped_len(text), per_byte, "{text:?}");
            let block = json!({
                "type": "text",
                "text": text,
                "extra\n": [1, "two\t"],
                "note": "n\"q"
            });
            assert_eq!(
                content_item_bytes(&block),
                serialized_len(&block) + 1,
                "{text:?}"
            );
        }
        let image = json!({ "type": "image", "data": "x" });
        assert_eq!(content_item_bytes(&image), 64);
    }
    #[test]
    fn output_budget_handles_escaped_utf8_text() {
        let controls: String = (0u8..=127).map(char::from).collect();
        let payload = format!("{controls}한글🙂\r\t").repeat(500);
        let response = normalize_tool_result(
            "file-read",
            RawResult::structured(payload, json!({ "bytes": 1 })),
            Duration::ZERO,
        );
        assert!(
            serde_json::to_vec(&response["structuredContent"])
                .unwrap()
                .len()
                <= max_output_bytes()
        );
        assert_eq!(response["_meta"]["fsMcpResult"]["outputTruncated"], true);
    }
    #[test]
    fn escape_heavy_body_keeps_content_instead_of_zero_bytes() {
        // 이스케이프 과다 본문 0바이트 회귀
        let payload = "let path = \"C:\\\\dir\\\\file\";\n".repeat(8_000);
        let response = normalize_tool_result(
            "file-read",
            RawResult::structured(payload, json!({ "bytes": 1 })),
            Duration::ZERO,
        );
        let standard = serde_json::to_vec(&response["structuredContent"]).unwrap();
        assert!(standard.len() <= max_output_bytes(), "{}", standard.len());
        let text = response["structuredContent"]["data"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(
            text.len() > max_output_bytes() / 2,
            "kept only {} bytes",
            text.len()
        );
        assert!(text.contains("[truncated: kept"));
    }
    #[test]
    fn normalizes_error_result() {
        let result = build_envelope("x", RawResult::error("boom"), Duration::from_millis(1));
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["structuredContent"]["error"]["message"],
            "Error: boom"
        );
        assert_eq!(result["_meta"]["fsMcpResult"]["status"], "error");
        // 오류 사유 노출
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Error: boom")
        );
    }
    #[test]
    fn item_scope_reports_exact_continuation_cursor() {
        let body = format!("C:/tmp/a.txt:\n{}", "abcdefghi\n".repeat(5_000));
        let mut raw = RawResult::structured(body, json!({ "path": "C:/tmp/a.txt", "offset": 100 }));
        enforce_payload_budget(&mut raw, 10_000, 1, TruncateScope::Item("file-read"));
        let text = raw.content[0]["text"].as_str().unwrap();
        let kept_chars = text
            .split_once('\n')
            .unwrap()
            .1
            .split("\n[truncated")
            .next()
            .unwrap()
            .chars()
            .count() as u64;
        assert!(
            text.contains(&format!("next: offset={}", 100 + kept_chars)),
            "{text}"
        );

        let lines: String = (1..=4_000)
            .map(|number| format!("{number}: line body\n"))
            .collect();
        let mut ranged = RawResult::structured(
            format!("C:/tmp/b.txt:\n{lines}"),
            json!({ "returned": 4_000 }),
        );
        enforce_payload_budget(
            &mut ranged,
            10_000,
            1,
            TruncateScope::Item("file-read-line-range"),
        );
        let text = ranged.content[0]["text"].as_str().unwrap();
        let last = text
            .split("\n[truncated")
            .next()
            .unwrap()
            .trim_end()
            .rsplit('\n')
            .next()
            .unwrap();
        let number: u64 = last.split_once(": ").unwrap().0.parse().unwrap();
        assert!(
            text.contains(&format!("next: start_line={}", number + 1)),
            "{text}"
        );

        // 줄 경계 스냅 범위보다 긴 줄이 잘리면 같은 줄부터 재안내
        let long_line = format!(
            "C:/tmp/c.txt:\n1: short\n2: {}\n3: tail",
            "y".repeat(20_000)
        );
        let mut cut = RawResult::structured(long_line, json!({ "returned": 3 }));
        enforce_payload_budget(
            &mut cut,
            10_000,
            1,
            TruncateScope::Item("file-read-line-range"),
        );
        let text = cut.content[0]["text"].as_str().unwrap();
        assert!(
            text.contains("next: start_line=2 (line 2 was cut"),
            "{}",
            &text[text.len().saturating_sub(300)..]
        );
    }
    #[test]
    fn output_budget_follows_env_overrides() {
        assert_eq!(
            resolve_output_budget(None, None, None, None),
            MAX_STANDARD_BYTES
        );
        assert_eq!(
            resolve_output_budget(None, None, Some("50000"), None),
            60_000
        );
        assert_eq!(
            resolve_output_budget(None, Some("12345"), Some("50000"), None),
            12_345
        );
        assert_eq!(
            resolve_output_budget(None, Some("10"), None, None),
            MIN_OUTPUT_BYTES
        );
        assert_eq!(
            resolve_output_budget(None, Some("junk"), None, None),
            MAX_STANDARD_BYTES
        );
        // config.toml max_bytes 최우선
        assert_eq!(
            resolve_output_budget(Some(40_000), None, Some("50000"), None),
            40_000
        );
        assert_eq!(
            resolve_output_budget(Some(40_000), Some("12345"), None, None),
            40_000
        );
        assert_eq!(
            resolve_output_budget(Some(1), None, None, None),
            MIN_OUTPUT_BYTES
        );
        // 호스트 기본값은 명시값 다음
        assert_eq!(resolve_output_budget(None, None, None, Some(9_000)), 9_000);
        assert_eq!(
            resolve_output_budget(None, None, Some("50000"), Some(9_000)),
            60_000
        );
        assert_eq!(
            resolve_output_budget(Some(40_000), None, None, Some(9_000)),
            40_000
        );
    }
    #[test]
    fn lossless_envelope_sends_plain_text_without_structured_content() {
        let body = "file-read: 1/1 succeeded\n\n- [1] OK\nC:\\tmp\\a.txt:\nline \"one\"\r\n";
        let structured = json!({
          "failedCount": 0,
          "results": [{ "data": { "backend": "native-rust", "bytes": 12, "path": "C:\\tmp\\a.txt" }, "index": 1, "ok": true }],
          "succeededCount": 1,
          "toolName": "file-read",
          "totalCount": 1
        });
        let result = build_text_envelope(
            "file-read",
            RawResult::structured(body, structured),
            Duration::from_millis(3),
        );
        assert!(result.get("structuredContent").is_none());
        assert!(result.get("isError").is_none());
        let text = result["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            text,
            format!("{body}\n[meta] {{\"results\":[{{\"bytes\":12}}]}}")
        );
        assert_eq!(result["_meta"]["fsMcpResult"]["durationMs"], 3);
        assert_eq!(result["_meta"]["fsMcpResult"]["compress"], "lossless");
    }
    #[test]
    fn lossless_envelope_omits_meta_line_when_text_covers_it() {
        let structured = json!({ "results": [{ "data": { "backend": "native-grep", "path": "C:/x" }, "index": 1, "ok": true }], "totalCount": 1 });
        let result = build_text_envelope(
            "dir-list",
            RawResult::structured("C:/x:\na.txt", structured),
            Duration::ZERO,
        );
        assert_eq!(result["content"][0]["text"], "C:/x:\na.txt");
    }
    #[test]
    fn lossless_envelope_keeps_item_positions_and_errors() {
        let structured = json!({ "results": [{ "data": { "path": "C:/a" }, "index": 1, "ok": true }, { "data": { "returned": 3 }, "index": 2, "ok": true }] });
        let result = build_text_envelope(
            "file-read",
            RawResult::structured("C:/a:\nx", structured),
            Duration::ZERO,
        );
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .ends_with("[meta] {\"results\":[null,{\"returned\":3}]}")
        );

        let failed = build_text_envelope("x", RawResult::error("boom"), Duration::ZERO);
        assert_eq!(failed["isError"], true);
        assert_eq!(failed["content"][0]["text"], "Error: boom");
        assert_eq!(failed["_meta"]["fsMcpResult"]["status"], "error");
    }
    #[test]
    fn lossless_envelope_keeps_image_blocks() {
        let raw = RawResult {
            content: vec![json!({ "type": "image", "data": "AAAA", "mimeType": "image/png" })],
            structured: Some(json!({ "bytes": 3 })),
            is_error: false,
            meta: Map::new(),
        };
        let result = build_text_envelope("file-read", raw, Duration::ZERO);
        assert_eq!(result["content"][0]["text"], "[meta] {\"bytes\":3}");
        assert_eq!(result["content"][1]["type"], "image");
    }
    #[test]
    fn display_text_has_no_ansi_escape_codes() {
        let standard = json!({ "data": { "structuredContent": { "totalCount": 5 } } });
        let display = create_display_text("file-read", "success", &standard, 1, true);
        assert_eq!(
            display,
            "-----------------------------------\n• tool = file-read\n• items = 5\n• status = success\n• truncated = true\n• duration = 0.001 sec\n-----------------------------------"
        );
        assert!(!display.contains('\u{1b}'));
    }
    #[test]
    fn compact_success_envelope_drops_static_fields() {
        let raw = RawResult::structured("body", json!({ "totalCount": 1 }));
        let result = build_envelope("x", raw, Duration::from_millis(1));
        let standard = &result["structuredContent"];
        assert!(standard.get("error").is_none());
        assert!(standard.get("schemaVersion").is_none());
        assert!(standard.get("status").is_none());
        assert!(standard.get("toolName").is_none());
        assert!(standard["durationMs"].is_u64());
        assert_eq!(standard["data"]["content"][0]["text"], "body");
    }
    #[test]
    fn output_budget_truncates_oversized_text() {
        let mut raw = RawResult::structured("x".repeat(50_000), json!({ "totalCount": 1 }));
        enforce_output_budget("file-read", &mut raw, 10_000, CompressMode::Off);
        let text = raw.content[0]["text"].as_str().unwrap();
        assert!(text.len() < 10_000, "{}", text.len());
        assert!(text.contains("[truncated: kept"));
        assert_eq!(raw.meta["outputTruncated"], Value::Bool(true));
    }
    #[test]
    fn output_budget_keeps_small_results_byte_identical() {
        let mut raw = RawResult::structured("small body", json!({ "totalCount": 1 }));
        enforce_output_budget("file-read", &mut raw, MAX_STANDARD_BYTES, CompressMode::Off);
        assert_eq!(raw.content[0]["text"], "small body");
        assert!(!raw.meta.contains_key("outputTruncated"));
    }
    #[test]
    fn output_budget_counts_error_text_copies() {
        let mut raw = RawResult::error("e".repeat(30_000));
        enforce_output_budget("file-read", &mut raw, 10_000, CompressMode::Off);
        let text = raw.content[0]["text"].as_str().unwrap().to_string();
        assert!(text.contains("[truncated: kept"));
        let envelope = build_envelope("x", raw, Duration::from_millis(1));
        let standard = serde_json::to_string(&envelope["structuredContent"]).unwrap();
        assert!(standard.len() <= 10_000, "{}", standard.len());
    }
    #[test]
    fn lossless_error_keeps_single_text_copy() {
        let mut lossless = RawResult::error("e".repeat(8_000));
        enforce_output_budget("git-commit", &mut lossless, 9_000, CompressMode::Lossless);
        assert!(!lossless.meta.contains_key("outputTruncated"));
        let mut off = RawResult::error("e".repeat(8_000));
        enforce_output_budget("git-commit", &mut off, 9_000, CompressMode::Off);
        assert_eq!(off.meta["outputTruncated"], Value::Bool(true));
    }
    #[test]
    fn output_budget_drops_structured_tail_when_text_is_small() {
        let results: Vec<Value> = (0..500)
      .map(|index| json!({ "index": index, "ok": true, "data": { "path": format!("C:/tmp/file-{index}.txt") } }))
      .collect();
        let mut raw =
            RawResult::structured("list", json!({ "results": results, "totalCount": 500 }));
        enforce_output_budget("file-read", &mut raw, 8_000, CompressMode::Off);
        let structured = raw.structured.as_ref().unwrap();
        let kept = structured["results"].as_array().unwrap().len() as u64;
        let dropped = structured["resultsDropped"].as_u64().unwrap();
        assert!(kept < 500);
        assert_eq!(kept + dropped, 500);
    }
    #[test]
    fn output_budget_exempts_image_blocks() {
        let mut raw = RawResult {
            content: vec![
                json!({ "type": "image", "data": "A".repeat(200_000), "mimeType": "image/png" }),
            ],
            structured: Some(json!({ "bytes": 200_000 })),
            is_error: false,
            meta: Map::new(),
        };
        enforce_output_budget("file-read", &mut raw, 10_000, CompressMode::Off);
        assert_eq!(raw.content[0]["data"].as_str().unwrap().len(), 200_000);
        assert!(!raw.meta.contains_key("outputTruncated"));
    }
    #[test]
    fn normalize_tool_result_truncates_oversized_bodies_under_budget() {
        let raw = RawResult::structured("y".repeat(400_000), json!({ "totalCount": 1 }));
        let result = normalize_tool_result("file-read", raw, Duration::from_millis(1));
        let standard = serde_json::to_string(&result["structuredContent"]).unwrap();
        assert!(standard.len() <= max_output_bytes(), "{}", standard.len());
        assert!(standard.contains("[truncated: kept"));
        assert_eq!(
            result["_meta"]["fsMcpResult"]["outputTruncated"],
            Value::Bool(true)
        );
    }
    #[test]
    fn normalize_content_passes_items_through_without_rewrapping() {
        // sanitizer가 identity인 동안 text 항목은 재래핑 없이 그대로 전달됨(본문 복사 제거).
        // 버킷 보이는 비텍스트 바이너리 바이트도 손실 없이 보존되어야 함.
        let image = json!({ "type": "image", "data": "AAAA", "mimeType": "image/png" });
        let normalized = normalize_content(vec![text_content("body".to_string()), image.clone()]);
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0]["type"], "text");
        assert_eq!(normalized[0]["text"], "body");
        assert_eq!(normalized[1], image);
    }
    #[test]
    fn normalize_content_fills_empty_with_one_text_block() {
        let normalized = normalize_content(Vec::new());
        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0]["type"], "text");
        assert_eq!(normalized[0]["text"], "");
    }
}
