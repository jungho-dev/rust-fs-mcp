//! fs_tools.rs
//! tools::fs_tools
//!
//! Collection of file / directory / metadata / image / HTTP read handlers.
//! Exposes the two edit variants file-edit (exact block replace) and file-edit-lines (1-based line range replace) together.
//!

use crate::core::args_ref::read_text_slice;
use crate::core::batch::{
    READ_PLAN, STAT_PLAN, create_batch_response, parallel_plan, run_batch_mutation,
    run_batch_parallel,
};
use crate::core::config::{ensure_path_allowed, target_path};
use crate::core::response::RawResult;
use base64::{Engine as _, engine::general_purpose};
use serde_json::{Map, Value, json};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::UNIX_EPOCH;

enum SliceRead {
    Text {
        content: String,
        line_count: usize,
        // Total file byte length. read_ascii_slice already scans to EOF so this is
        // returned alongside, sparing callers a separate fs::metadata call.
        byte_size: u64,
    },
    Binary(Vec<u8>),
    NonAscii,
}
// 1. Read tools ---------------------------------------------------------------
pub fn handle_file_read(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let items = read_items(args);
    if items.is_empty() {
        return RawResult::error(
            "paths or items is required (e.g. {\"paths\":[\"C:/absolute/file\"]})",
        );
    }
    let results = run_batch_parallel(&items, READ_PLAN, move |item| read_item(item, allow_missing));
    create_batch_response("file-read", results, true)
}
pub fn handle_file_read_line_range(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let items = read_items(args);
    if items.is_empty() {
        return RawResult::error(
            "paths or items is required (e.g. {\"paths\":[\"C:/absolute/file\"]})",
        );
    }
    let results = run_batch_parallel(&items, READ_PLAN, move |item| lines_item(item, allow_missing));
    create_batch_response("file-read-line-range", results, true)
}
fn read_items(args: &Value) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(paths) = args.get("paths").and_then(Value::as_array) {
        items.extend(
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(|path| json!({ "path": path })),
        );
    }
    if let Some(raw_items) = args.get("items").and_then(Value::as_array) {
        items.extend(raw_items.iter().cloned());
    }
    items
}
// Per-item whole-file cap. Held below the envelope output budget (core::response) so one
// capped read still fits a single response after JSON escaping; explicit offset/length
// requests are honored exactly.
fn read_max_chars() -> usize {
    80_000
}
fn read_item(item: &Value, allow_missing: bool) -> RawResult {
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
        match read_ascii_slice(&path, offset, length) {
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
            Ok(SliceRead::Binary(bytes)) => return binary_result(&path, bytes),
            Ok(SliceRead::NonAscii) => {}
            Err(error) => return RawResult::error(error),
        }
    }
    // Whole-file reads far past the cap stream through read_ascii_slice instead of
    // fs::read-ing the entire file: only the first max_chars stay in memory while
    // bytes/lineCount still cover the whole file. Non-ASCII files fall back below.
    let max_chars = read_max_chars();
    if max_chars > 0
        && offset == 0
        && length.is_none()
        && metadata.len() > (max_chars as u64) * 2
    {
        match read_ascii_slice(&path, 0, Some(max_chars)) {
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
            Ok(SliceRead::Binary(bytes)) => return binary_result(&path, bytes),
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

    if bytes.contains(&0) {
        return binary_result(&path, bytes);
    }
    let byte_len = bytes.len();
    // For valid UTF-8 from_utf8 takes ownership of `bytes` directly, avoiding a second heap copy.
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(&error.into_bytes()).into_owned(),
    };

    // Replaces the line-slice cost of text.lines().count() with a single byte-count pass.
    let lf_count = text.bytes().filter(|byte| *byte == b'\n').count();
    let line_count = if text.is_empty() {
        0
    }
    else if text.ends_with('\n') {
        lf_count
    }
    else {
        lf_count + 1
    };

    // A whole-file read (no explicit length) past read_max_chars is capped so the envelope stays
    // within client token limits; an explicit length is always honored exactly as requested.
    // chars().count() <= len(), so a byte-length pre-check skips the full char scan for files
    // that cannot exceed the cap — the common case.
    let maybe_over_cap = length.is_none() && max_chars > 0 && text.len().saturating_sub(offset) > max_chars;
    let total_chars = if maybe_over_cap {
        text.chars().count()
    }
    else {
        0
    };
    let truncated = maybe_over_cap && total_chars.saturating_sub(offset) > max_chars;
    let effective_length = if truncated { Some(max_chars) } else { length };
    let sliced = slice_chars(&text, offset, effective_length);

    let mut structured = json!({
        "path": path.display().to_string(),
        "bytes": byte_len,
        "lineCount": line_count
    });
    let body = if truncated {
        structured["truncated"] = json!(true);
        structured["returnedChars"] = json!(max_chars);
        structured["totalChars"] = json!(total_chars);
        format!(
            "{}:\n{}\n[truncated: returned {max_chars} of {total_chars} chars; pass offset/length to read more]",
            path.display(),
            sliced
        )
    }
    else {
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
    let sliced = slice_chars(
        &content,
        usize_field(item, "offset", 0),
        item.get("length")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
    );

    RawResult::structured(
        format!("{url}:\n{sliced}"),
        json!({
            "url": url,
            "finalUrl": page.final_url,
            "status": page.status,
            "contentType": page.content_type,
            "bytes": page.body.len()
        }),
    )
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
fn lines_item(item: &Value, allow_missing: bool) -> RawResult {
    let allow_missing = allow_missing || bool_field(item, "allowMissing", false);
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
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
        return RawResult::error(format!("Path is a directory: {}", path.display()));
    }
    let start_line = usize_field(item, "start_line", 1);
    if start_line == 0 {
        return RawResult::error("start_line is 1-based and must be >= 1");
    }
    let line_count = item
        .get("line_count")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    if line_count == Some(0) {
        return RawResult::error("line_count must be >= 1");
    }
    let (body, returned) = match read_lines_native(&path, start_line, line_count) {
        Ok(read) => read,
        Err(error) => {
            return RawResult::error(error);
        }
    };

    lines_result(&path, body, returned, Some("native-rust"))
}
fn lines_result(path: &Path, body: String, returned: usize, backend: Option<&str>) -> RawResult {
    // The numbered body ships in text once; the previous structured.lines array re-sent every
    // line wrapped in {number,text} objects, more than doubling the payload.
    let mut structured = json!({
        "path": path.display().to_string(),
        "returned": returned
    });
    if let Some(backend) = backend {
        structured["backend"] = json!(backend);
    }
    let mut result = RawResult::structured(body, structured);
    if let Some(backend) = backend {
        result.meta.insert("backend".to_string(), json!(backend));
    }
    result
}
// Builds the "{path}:\n{n}: {line}..." body in ONE pre-grown String while scanning: the
// previous Vec<(usize,String)> + per-line format! + join allocated three times per line.
fn read_lines_native(
    path: &Path,
    start_line: usize,
    line_count: Option<usize>,
) -> Result<(String, usize), String> {
    let file = fs::File::open(path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line_number = 0usize;
    let mut skipped = Vec::new();
    let path_text = path.display().to_string();
    // go-fs-mcp와 동일한 예상 용량: 제한된 요청은 줄당 ~24B, 무제한은 4KB에서 시작.
    let grow = match line_count {
        Some(limit) if limit <= 8192 => limit * 24,
        _ => 4096,
    };
    let mut body = String::with_capacity(path_text.len() + 2 + grow);
    body.push_str(&path_text);
    body.push_str(":\n");
    while line_number + 1 < start_line {
        skipped.clear();
        let read = reader
            .read_until(b'\n', &mut skipped)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if read == 0 {
            return Ok((body, 0));
        }
        line_number += 1;
    }
    let mut line = String::new();
    let mut returned = 0usize;
    loop {
        if let Some(line_count) = line_count && returned >= line_count
        {
            break;
        }
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        if returned > 0 {
            body.push('\n');
        }
        push_usize(&mut body, line_number + 1);
        body.push_str(": ");
        body.push_str(&line);
        returned += 1;
        line_number += 1;
    }
    Ok((body, returned))
}
// 스택 버퍼 숫자 포매팅: 줄마다 fmt 기계를 타지 않는다(go strconv.AppendUint 대응).
fn push_usize(buf: &mut String, value: usize) {
    let mut digits = [0u8; 20];
    let mut index = digits.len();
    let mut rest = value;
    loop {
        index -= 1;
        digits[index] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    buf.push_str(std::str::from_utf8(&digits[index..]).unwrap_or("0"));
}
// 2. Write and directory tools ------------------------------------------------
pub fn handle_file_write(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    // 독립 경로만 건드리는 mutation 배치는 병렬(충돌·조상 관계는 순차 폴백).
    let results = run_batch_mutation(items, write_touched, write_item);
    create_batch_response("file-write", results, false)
}
pub fn handle_dir_create(args: &Value) -> RawResult {
    let Some(paths) = args.get("paths").and_then(Value::as_array) else {
        return RawResult::error("paths must be an array (e.g. paths:[\"C:/absolute/path\"])");
    };

    let items = paths
        .iter()
        .filter_map(Value::as_str)
        .map(|path| json!({ "path": path }))
        .collect::<Vec<_>>();
    let results = run_batch_mutation(&items, path_touched, mkdir_item);
    create_batch_response("dir-create", results, false)
}
pub fn handle_dir_list(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_parallel(items, parallel_plan(), move |item| list_dir_item(item, allow_missing));
    create_batch_response("dir-list", results, true)
}
fn write_item(item: &Value) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let content = match item_content(item, "content", "content_path") {
        Ok(content) => content,
        Err(error) => return RawResult::error(error),
    };
    let path = match target_path(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if let Err(error) = ensure_parent_dir(&path) {
        return RawResult::error(error);
    }
    let mode = item
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("rewrite");
    let result = if mode == "append" {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(content.as_bytes()))
    }
    else {
        write_atomic(&path, content.as_bytes())
    };

    match result {
        Ok(()) => RawResult::structured(
            format!("Wrote {} bytes to {}", content.len(), path.display()),
            json!({
                "path": path.display().to_string(),
                "bytes": content.len(),
                "mode": mode
            }),
        ),
        Err(error) => RawResult::error(format!("Failed to write {}: {error}", path.display())),
    }
}
fn mkdir_item(item: &Value) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match target_path(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    match fs::create_dir_all(&path) {
        Ok(()) => RawResult::structured(
            format!("Created directory {}", path.display()),
            json!({ "path": path.display().to_string() }),
        ),
        Err(error) => RawResult::error(format!("Failed to create {}: {error}", path.display())),
    }
}
fn list_dir_item(item: &Value, allow_missing: bool) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let Ok(metadata) = fs::metadata(&path) else {
        if allow_missing {
            return RawResult::structured(
                format!("Missing directory: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        return RawResult::error(format!("Directory does not exist: {}", path.display()));
    };
    if !metadata.is_dir() {
        return RawResult::error(format!("Path is not a directory: {}", path.display()));
    }
    let depth = usize_field(item, "depth", 2);
    let max_entries = usize_field(item, "maxEntries", 500);
    let include_files = bool_field(item, "includeFiles", true);
    let excludes = item
        .get("excludePatterns")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if max_entries == 0 {
        return dir_list_result(&path, Vec::new(), true, None);
    }
    // Collect max+1 entries so "exactly max" and "actually truncated" are distinguishable
    // (the previous len >= max check reported a false positive at the boundary).
    let (mut entries, backend) = match list_dir_native(&path, depth, max_entries.saturating_add(1), include_files, &excludes) {
            Ok(entries) => (entries, Some("native-rust")),
            Err(error) => return RawResult::error(error),
        };
    let truncated = entries.len() > max_entries;
    entries.truncate(max_entries);

    dir_list_result(&path, entries, truncated, backend)
}
fn dir_list_result(
    path: &Path,
    entries: Vec<String>,
    truncated: bool,
    backend: Option<&str>,
) -> RawResult {
    // The entry list ships in the text body once; structured carries metadata only instead of
    // re-sending every entry as a JSON array.
    let cap = entries.iter().map(|item| item.len() + 1).sum::<usize>();
    let mut text = String::with_capacity(cap);
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str(entry);
    }
    let mut structured = json!({
        "path": path.display().to_string(),
        "entryCount": entries.len(),
        "truncated": truncated
    });
    if let Some(backend) = backend {
        structured["backend"] = json!(backend);
    }
    let mut result = RawResult::structured(format!("{}:\n{}", path.display(), text), structured);
    if let Some(backend) = backend {
        result.meta.insert("backend".to_string(), json!(backend));
    }
    result
}
fn list_dir_native(
    path: &Path,
    depth: usize,
    max_entries: usize,
    include_files: bool,
    excludes: &[String],
) -> Result<Vec<String>, String> {
    let mut entries = Vec::new();
    if depth == 0 || max_entries == 0 {
        return Ok(entries);
    }
    let excludes = ExcludeSet::new(excludes);
    let ctx = DirCollect {
        root: path,
        max_depth: depth,
        max_entries,
        include_files,
        excludes: &excludes,
    };

    collect_dir_entries(&ctx, path, 1, &mut entries)?;
    entries.sort();
    Ok(entries)
}
struct DirCollect<'a> {
    root: &'a Path,
    max_depth: usize,
    max_entries: usize,
    include_files: bool,
    excludes: &'a ExcludeSet,
}
fn collect_dir_entries(
    ctx: &DirCollect<'_>,
    dir: &Path,
    current_depth: usize,
    entries: &mut Vec<String>,
) -> Result<(), String> {
    if entries.len() >= ctx.max_entries {
        return Ok(());
    }
    let mut dir_entries = fs::read_dir(dir)
        .map_err(|error| format!("Failed to list {}: {error}", dir.display()))? .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    dir_entries.sort_by_key(|entry| entry.path());

    let mut children = Vec::new();
    for entry in dir_entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        let is_dir = file_type.is_dir();
        let is_file = file_type.is_file();
        let Some(rel) = native_entry_name(ctx.root, &path, is_dir) else {
            continue;
        };
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if ctx.excludes.matches(&rel, name, is_dir) {
            continue;
        }
        if is_dir {
            entries.push(rel);
            if current_depth < ctx.max_depth {
                children.push(path);
            }
        }
        else if ctx.include_files && is_file {
            entries.push(rel);
        }
        if entries.len() >= ctx.max_entries {
            return Ok(());
        }
    }
    for child in children {
        collect_dir_entries(ctx, &child, current_depth + 1, entries)?;
        if entries.len() >= ctx.max_entries {
            return Ok(());
        }
    }
    Ok(())
}
struct ExcludeSet {
    patterns: Vec<ExcludePattern>,
}
impl ExcludeSet {
    fn new(patterns: &[String]) -> Self {
        Self {
            patterns: patterns
                .iter()
                .map(|pattern| pattern.replace('\\', "/"))
                .filter(|pattern| !pattern.trim().is_empty())
                .map(ExcludePattern::new)
                .collect(),
        }
    }
    fn matches(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        self.patterns
            .iter()
            .any(|pattern| pattern.matches(rel, name, is_dir))
    }
}
struct ExcludePattern {
    text: String,
    dir_only: bool,
    glob: CompiledWildcard,
    rest: Option<ExcludeRest>,
}
struct ExcludeRest {
    text: String,
    glob: CompiledWildcard,
}
impl ExcludePattern {
    fn new(pattern: String) -> Self {
        let pattern = pattern.trim().to_string();
        let dir_only = pattern.ends_with('/');
        let text = pattern.trim_matches('/').to_string();
        let rest = text.strip_prefix("**/").map(|rest| ExcludeRest {
            text: rest.to_string(),
            glob: CompiledWildcard::new(rest),
        });
        Self {
            glob: CompiledWildcard::new(&text),
            text,
            dir_only,
            rest,
        }
    }
    fn matches(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let rel = rel.trim_end_matches('/');
        if self.text == rel || self.text == name {
            return true;
        }
        if let Some(rest) = &self.rest && (rest.text == name || rest.glob.matches(name) || rest.glob.matches(rel))
        {
            return true;
        }
        self.glob.matches(rel) || self.glob.matches(name)
    }
}
struct CompiledWildcard {
    pattern_chars: Vec<char>,
    // When the pattern is all ASCII compare against byte slices directly without a per-match `Vec<char>` allocation.
    pattern_bytes: Option<Vec<u8>>,
}
impl CompiledWildcard {
    fn new(pattern: &str) -> Self {
        let pattern_bytes = pattern.is_ascii().then(|| pattern.as_bytes().to_vec());
        Self {
            pattern_chars: pattern.chars().collect(),
            pattern_bytes,
        }
    }
    fn matches(&self, value: &str) -> bool {
        if let Some(bytes) = &self.pattern_bytes && value.is_ascii()
        {
            return match_glob_bytes(bytes, value.as_bytes());
        }
        let value_chars: Vec<char> = value.chars().collect();
        match_glob_chars(&self.pattern_chars, &value_chars)
    }
}
fn match_glob_bytes(pattern: &[u8], value: &[u8]) -> bool {
    let mut pi = 0usize;
    let mut vi = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_vi = 0usize;

    while vi < value.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == value[vi]) {
            pi += 1;
            vi += 1;
        }
        else if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = Some(pi);
            star_vi = vi;
            pi += 1;
        }
        else if let Some(index) = star_pi {
            pi = index + 1;
            star_vi += 1;
            vi = star_vi;
        }
        else {
        	return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}
fn match_glob_chars(pattern: &[char], value: &[char]) -> bool {
    let mut pi = 0usize;
    let mut vi = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_vi = 0usize;

    while vi < value.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == value[vi]) {
            pi += 1;
            vi += 1;
        }
        else if pi < pattern.len() && pattern[pi] == '*' {
            star_pi = Some(pi);
            star_vi = vi;
            pi += 1;
        }
        else if let Some(index) = star_pi {
            pi = index + 1;
            star_vi += 1;
            vi = star_vi;
        }
        else {
        	return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}
fn native_entry_name(root: &Path, path: &Path, is_dir: bool) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    let cow = rel.to_string_lossy();
    let needs_replace = cfg!(windows) && cow.contains('\\');
    let extra = if is_dir { 1 } else { 0 };
    let mut out = String::with_capacity(cow.len() + extra);
    if needs_replace {
        for ch in cow.chars() {
            out.push(if ch == '\\' { '/' } else { ch });
        }
    }
    else {
    	out.push_str(&cow);
    }
    if is_dir {
        out.push('/');
    }
    Some(out)
}
// 3. Copy, move, remove, metadata, edit --------------------------------------
pub fn handle_path_copy(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_mutation(items, transfer_touched, copy_item);
    create_batch_response("path-copy", results, false)
}
pub fn handle_path_move(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_mutation(items, transfer_touched, move_item);
    create_batch_response("path-move", results, false)
}
pub fn handle_path_remove(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_mutation(items, path_touched, remove_item);
    create_batch_response("path-remove", results, false)
}
pub fn handle_path_stat(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let Some(paths) = args.get("paths").and_then(Value::as_array) else {
        return RawResult::error("paths must be an array (e.g. paths:[\"C:/absolute/path\"])");
    };

    let items = paths
        .iter()
        .filter_map(Value::as_str)
        .map(|path| json!({ "path": path }))
        .collect::<Vec<_>>();
    let results = run_batch_parallel(&items, STAT_PLAN, move |item| info_item(item, allow_missing));
    create_batch_response("path-stat", results, false)
}
pub fn handle_file_edit(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_mutation(items, edit_touched, edit_item);
    create_batch_response("file-edit", results, false)
}
pub fn handle_file_edit_lines(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_mutation(items, edit_lines_touched, edit_lines_item);
    create_batch_response("file-edit-lines", results, false)
}
fn copy_item(item: &Value) -> RawResult {
    let Some(source) = item.get("source").and_then(Value::as_str) else {
        return RawResult::error("source must be a string");
    };
    let Some(destination) = item.get("destination").and_then(Value::as_str) else {
        return RawResult::error("destination must be a string");
    };

    let source = match ensure_path_allowed(source) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let Ok(source_meta) = fs::metadata(&source) else {
        return RawResult::error(format!("Path does not exist: {}", source.display()));
    };
    let destination = match target_path(destination) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let recursive = bool_field(item, "recursive", false);
    let force = bool_field(item, "force", false);
    if destination.exists() && !force {
        return RawResult::error(format!("Destination exists: {}", destination.display()));
    }
    let result = if source_meta.is_dir() {
        if !recursive {
            return RawResult::error("recursive must be true to copy a directory");
        }
        copy_dir_recursive(&source, &destination)
    }
    else {
        if let Err(error) = ensure_parent_dir(&destination) {
            return RawResult::error(error);
        }
        fs::copy(&source, &destination)
            .map(|_| ())
            .map_err(|error| error.to_string())
    };

    match result {
        Ok(()) => RawResult::structured(
            format!("Copied {} -> {}", source.display(), destination.display()),
            json!({
                "source": source.display().to_string(),
                "destination": destination.display().to_string()
            }),
        ),
        Err(error) => RawResult::error(format!("Failed to copy: {error}")),
    }
}
fn move_item(item: &Value) -> RawResult {
    let Some(source) = item.get("source").and_then(Value::as_str) else {
        return RawResult::error("source must be a string");
    };
    let Some(destination) = item.get("destination").and_then(Value::as_str) else {
        return RawResult::error("destination must be a string");
    };

    let source = match ensure_path_allowed(source) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if fs::metadata(&source).is_err() {
        return RawResult::error(format!("Path does not exist: {}", source.display()));
    }
    let destination = match target_path(destination) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if let Err(error) = ensure_parent_dir(&destination) {
        return RawResult::error(error);
    }
    let moved = match fs::rename(&source, &destination) {
        Ok(()) => Ok(()),
        // rename cannot cross volumes (C: -> D: fails with NOT_SAME_DEVICE / EXDEV),
        // so fall back to copy + remove-source for cross-device moves.
        Err(error) if is_cross_device(&error) => copy_then_remove(&source, &destination),
        Err(error) => Err(error.to_string()),
    };
    match moved {
        Ok(()) => RawResult::structured(
            format!("Moved {} -> {}", source.display(), destination.display()),
            json!({
                "source": source.display().to_string(),
                "destination": destination.display().to_string()
            }),
        ),
        Err(error) => RawResult::error(format!("Failed to move: {error}")),
    }
}
fn remove_item(item: &Value) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let force = bool_field(item, "force", false);
    let Ok(metadata) = fs::metadata(&path) else {
        if force {
            return RawResult::structured(
                format!("Already absent: {}", path.display()),
                json!({ "path": path.display().to_string(), "removed": false }),
            );
        }
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    };
    let result = if metadata.is_dir() {
        if !bool_field(item, "recursive", false) {
            return RawResult::error("recursive must be true to remove a directory");
        }
        fs::remove_dir_all(&path)
    }
    else {
        fs::remove_file(&path)
    };

    match result {
        Ok(()) => RawResult::structured(
            format!("Removed {}", path.display()),
            json!({ "path": path.display().to_string(), "removed": true }),
        ),
        Err(error) => RawResult::error(format!("Failed to remove {}: {error}", path.display())),
    }
}
fn info_item(item: &Value, allow_missing: bool) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };
    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };

    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(_) if allow_missing => {
            return RawResult::structured(
                format!("Missing path: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        Err(_) => {
            return RawResult::error(format!("Path does not exist: {}", path.display()));
        }
    };

    RawResult::structured(
        format!("{}: {} bytes", path.display(), metadata.len()),
        json!({
            "path": path.display().to_string(),
            "isDirectory": metadata.is_dir(),
            "isFile": metadata.is_file(),
            "len": metadata.len(),
            "readonly": metadata.permissions().readonly(),
            "modified": timestamp(metadata.modified().ok()),
            "created": timestamp(metadata.created().ok()),
            "accessed": timestamp(metadata.accessed().ok())
        }),
    )
}
fn edit_item(item: &Value) -> RawResult {
    let Some(path) = item.get("file_path").and_then(Value::as_str) else {
        return RawResult::error("file_path must be a string");
    };
    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let Ok(metadata) = fs::metadata(&path) else {
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    };
    if metadata.is_dir() {
        return RawResult::error(format!("Path is a directory: {}", path.display()));
    }
    let old_string = match item_content(item, "old_string", "old_string_path") {
        Ok(content) => content,
        Err(error) => return RawResult::error(error),
    };
    let new_string = match item_content(item, "new_string", "new_string_path") {
        Ok(content) => content,
        Err(error) => return RawResult::error(error),
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return RawResult::error(format!("Failed to read {}: {error}", path.display()));
        }
    };

    let (effective_old, effective_new, eol_mode) = resolve_edit_strings(&text, &old_string, &new_string);
    // 빈 old_string은 모든 문자 경계에 삽입되어 파일 전체를 손상시키므로 거부.
    if effective_old.is_empty() {
        return RawResult::error("old_string must not be empty");
    }
    // Single pass: build the edited body while counting matches, instead of scanning the
    // whole text twice via matches().count() followed by replace().
    let mut edited = String::with_capacity(text.len());
    let mut count = 0usize;
    let mut rest = text.as_str();
    while let Some(found) = rest.find(&effective_old) {
        edited.push_str(&rest[..found]);
        edited.push_str(&effective_new);
        rest = &rest[found + effective_old.len()..];
        count += 1;
    }
    edited.push_str(rest);
    if count == 0 {
        return RawResult::error("old_string was not found");
    }
    match item.get("expected_replacements").and_then(Value::as_u64) {
        Some(expected) if count != expected as usize => {
            return RawResult::error(format!(
                "Expected {expected} replacements but found {count}"
            ));
        }
        _ => {}
    }
    if let Err(error) = write_atomic(&path, edited.as_bytes()) {
        return RawResult::error(format!("Failed to write {}: {error}", path.display()));
    }
    RawResult::structured(
        format!("Edited {} ({count} replacements)", path.display()),
        json!({
            "file_path": path.display().to_string(),
            "replacements": count,
            "bytes": edited.len(),
            "eolMode": eol_mode
        }),
    )
}
fn edit_lines_item(item: &Value) -> RawResult {
    let Some(path) = item.get("file_path").and_then(Value::as_str) else {
        return RawResult::error("file_path must be a string");
    };
    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let Ok(metadata) = fs::metadata(&path) else {
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    };
    if metadata.is_dir() {
        return RawResult::error(format!("Path is a directory: {}", path.display()));
    }
    let Some(start_line) = item.get("start_line").and_then(Value::as_u64) else {
        return RawResult::error("start_line must be a positive integer");
    };
    if start_line == 0 {
        return RawResult::error("start_line is 1-based and must be >= 1");
    }
    let end_line = item
        .get("end_line")
        .and_then(Value::as_u64)
        .unwrap_or(start_line);
    if end_line < start_line {
        return RawResult::error("end_line must be >= start_line");
    }
    let after = bool_field(item, "after", false);
    let replacement = match item.get("replacement").and_then(Value::as_str) {
        Some(value) => value.to_string(),
        None => match item_content(item, "replacement", "replacement_path") {
            Ok(value) => value,
            Err(error) => return RawResult::error(error),
        },
    };

    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return RawResult::error(format!("Failed to read {}: {error}", path.display()));
        }
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => return RawResult::error("file is not valid UTF-8"),
    };

    let eol = detect_dominant_eol(text);
    let line_ranges = compute_line_ranges(text);
    let total_lines = line_ranges.len();

    // expected_lines는 파일 전체 줄 수 드리프트 가드. 실사용 오류 전수가 "교체 범위 길이"로
    // 값을 준 경우라 범위 길이(end_line-start_line+1) 일치도 통과시킨다.
    if let Some(expected) = item.get("expected_lines").and_then(Value::as_u64) && total_lines as u64 != expected && end_line - start_line + 1 != expected
    {
        return RawResult::error(format!(
            "Expected {expected} lines but file has {total_lines} (expected_lines is the whole-file line count; the selected range {start_line}-{end_line} spans {} lines)",
            end_line - start_line + 1
        ));
    }
    if start_line as usize > total_lines && !after {
        return RawResult::error(format!(
            "start_line {start_line} exceeds file line count {total_lines}"
        ));
    }
    let effective_end = (end_line as usize).min(total_lines.max(1));
    let start_idx = start_line as usize - 1;
    let end_idx = effective_end.saturating_sub(1);

    let normalized = normalize_replacement_eol(&replacement, eol);
    let trailing_eol_needed = !normalized.is_empty() && !ends_with_eol(&normalized);
    // replace 모드: 교체 대상이 EOL로 끝날 때만 EOL 보존. 후행 개행 없는 마지막 줄 교체 시 추가 금지.
    let append_eol = if after {
        true
    }
    else {
        text[..line_ranges[end_idx].1].ends_with('\n')
    };
    let final_replacement = if trailing_eol_needed && append_eol {
        let mut value = normalized;
        value.push_str(eol);
        value
    }
    else {
        normalized
    };

    let mut new_text = String::with_capacity(text.len() + final_replacement.len());
    if after {
        let insert_byte = if total_lines == 0 {
            0
        }
        else if end_idx < total_lines {
            line_ranges[end_idx].1
        }
        else {
            text.len()
        };
        new_text.push_str(&text[..insert_byte]);
        let needs_eol_before = insert_byte > 0 && !text[..insert_byte].ends_with('\n') && !final_replacement.is_empty();
        if needs_eol_before {
            new_text.push_str(eol);
        }
        new_text.push_str(&final_replacement);
        new_text.push_str(&text[insert_byte..]);
    }
    else {
    	let cut_start = line_ranges[start_idx].0;
        let cut_end = line_ranges[end_idx].1;
        new_text.push_str(&text[..cut_start]);
        new_text.push_str(&final_replacement);
        new_text.push_str(&text[cut_end..]);
    }
    if let Err(error) = write_atomic(&path, new_text.as_bytes()) {
        return RawResult::error(format!("Failed to write {}: {error}", path.display()));
    }
    let action = if after {
        "insert_after"
    }
    else if final_replacement.is_empty() {
        "delete"
    }
    else {
        "replace"
    };
    let lines_changed = if after {
        0
    }
    else {
        effective_end - start_line as usize + 1
    };

    RawResult::structured(
        format!(
            "{action} {} (lines {start_line}-{effective_end})",
            path.display()
        ),
        json!({
            "file_path": path.display().to_string(),
            "action": action,
            "start_line": start_line,
            "end_line": effective_end,
            "lines_removed": lines_changed,
            "bytes": new_text.len(),
            "eol": match eol { "\r\n" => "crlf", _ => "lf" }
        }),
    )
}
fn detect_dominant_eol(text: &str) -> &'static str {
    // Count CRLF and LF in a single pass. Previously text.matches() was called twice,
    // scanning the whole string twice; this collapses it to one pass.
    let bytes = text.as_bytes();
    let mut crlf = 0usize;
    let mut lf_only = 0usize;
    for index in 0..bytes.len() {
        if bytes[index] != b'\n' {
            continue;
        }
        if index > 0 && bytes[index - 1] == b'\r' {
            crlf += 1;
        }
        else {
        	lf_only += 1;
        }
    }
    if crlf >= lf_only && crlf > 0 {
        "\r\n"
    }
    else {
    	"\n"
    }
}
fn compute_line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            ranges.push((start, index + 1));
            start = index + 1;
        }
    }
    if start < bytes.len() {
        ranges.push((start, bytes.len()));
    }
    ranges
}
fn ends_with_eol(value: &str) -> bool {
    value.ends_with('\n') || value.ends_with('\r')
}
fn normalize_replacement_eol(value: &str, target_eol: &str) -> String {
    if target_eol == "\n" {
        return crlf_to_lf(value);
    }
    let lf_only = crlf_to_lf(value);
    lf_to_crlf(&lf_only)
}
fn resolve_edit_strings(text: &str, old: &str, new: &str) -> (String, String, &'static str) {
    if text.contains(old) {
        return (old.to_string(), new.to_string(), "raw");
    }
    let file_has_crlf = text.contains("\r\n");
    let old_has_crlf = old.contains("\r\n");
    let new_has_crlf = new.contains("\r\n");
    if file_has_crlf && !old_has_crlf {
        let new_old = lf_to_crlf(old);
        if text.contains(&new_old) {
            let new_new = if new_has_crlf {
                new.to_string()
            }
            else {
                lf_to_crlf(new)
            };
            return (new_old, new_new, "lf_to_crlf");
        }
    }
    if !file_has_crlf && old_has_crlf {
        let new_old = crlf_to_lf(old);
        if text.contains(&new_old) {
            let new_new = if new_has_crlf {
                crlf_to_lf(new)
            }
            else {
                new.to_string()
            };
            return (new_old, new_new, "crlf_to_lf");
        }
    }
    (old.to_string(), new.to_string(), "raw")
}
fn lf_to_crlf(value: &str) -> String {
    // Drops the matches('\n').count() pre-scan and uses a heuristic capacity (12.5% headroom) for a single pass.
    let mut out = String::with_capacity(value.len() + value.len() / 8);
    let mut prev_was_cr = false;
    for ch in value.chars() {
        if ch == '\n' && !prev_was_cr {
            out.push('\r');
            out.push('\n');
        }
        else {
        	out.push(ch);
        }
        prev_was_cr = ch == '\r';
    }
    out
}
fn crlf_to_lf(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' && chars.peek() == Some(&'\n') {
            chars.next();
            out.push('\n');
        }
        else {
        	out.push(ch);
        }
    }
    out
}
// 4. Shared helpers -----------------------------------------------------------
// Mutation 배치의 독립성 판정에 쓰이는 touched-path 추출기(go pathTouched 계열 대응).
// 필수 키가 없으면 빈 벡터를 돌려줘 순차 폴백을 유도한다.
fn string_field_vec(item: &Value, keys: &[&str], required: &str) -> Vec<String> {
    if item.get(required).and_then(Value::as_str).is_none() {
        return Vec::new();
    }
    keys.iter().filter_map(|key| item.get(*key).and_then(Value::as_str).map(str::to_string)).collect()
}
fn path_touched(item: &Value) -> Vec<String> {
    string_field_vec(item, &["path"], "path")
}
fn write_touched(item: &Value) -> Vec<String> {
    string_field_vec(item, &["path", "content_path"], "path")
}
fn transfer_touched(item: &Value) -> Vec<String> {
    let touched = string_field_vec(item, &["source", "destination"], "source");
    if touched.len() < 2 {
        return Vec::new();
    }
    touched
}
fn edit_touched(item: &Value) -> Vec<String> {
    string_field_vec(item, &["file_path", "old_string_path", "new_string_path"], "file_path")
}
fn edit_lines_touched(item: &Value) -> Vec<String> {
    string_field_vec(item, &["file_path", "replacement_path"], "file_path")
}
fn ensure_parent_dir(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create {}: {error}", parent.display()))
}
// Rewrite-mode writes go through a same-directory temp file + rename so an interrupted
// write never leaves the destination truncated. rename replaces an existing file on both
// Windows (MOVEFILE_REPLACE_EXISTING) and POSIX. When the temp file cannot be created or
// the final rename fails (e.g. exotic permissions), fall back to the direct write so the
// success envelope stays identical to the previous behavior.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) else {
        return fs::write(path, bytes);
    };
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let stamp = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let mut temp = None;
    for attempt in 0u32..16 {
        let candidate = parent.join(format!(".{name}.{stamp}-{attempt}.fsmcp.tmp"));
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(file) => {
                temp = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => break,
        }
    }
    let Some((temp_path, mut file)) = temp else {
        return fs::write(path, bytes);
    };
    let written = file.write_all(bytes).and_then(|_| file.flush());
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    match fs::rename(&temp_path, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = fs::remove_file(&temp_path);
            fs::write(path, bytes)
        }
    }
}
// Windows ERROR_NOT_SAME_DEVICE(17) / POSIX EXDEV(18): rename refused across volumes.
fn is_cross_device(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::CrossesDevices {
        return true;
    }
    match error.raw_os_error() {
        Some(17) => cfg!(windows),
        Some(18) => !cfg!(windows),
        _ => false,
    }
}
fn copy_then_remove(source: &Path, destination: &Path) -> Result<(), String> {
    if source.is_dir() {
        copy_dir_recursive(source, destination)?;
        fs::remove_dir_all(source).map_err(|error| format!("copied but failed to remove source {}: {error}", source.display()))
    }
    else {
        fs::copy(source, destination)
            .map(|_| ())
            .map_err(|error| error.to_string())?;
        fs::remove_file(source).map_err(|error| format!("copied but failed to remove source {}: {error}", source.display()))
    }
}
fn item_content(item: &Value, inline_key: &str, path_key: &str) -> Result<String, String> {
    if let Some(path) = item.get(path_key).and_then(Value::as_str) {
        let path = ensure_path_allowed(path)?;
        // 스키마 키는 `<inline_key>_offset/_length`. replacement 등 string/content 미포함 키도 포함.
        let offset_key = format!("{inline_key}_offset");
        let length_key = format!("{inline_key}_length");
        let offset = item.get(&offset_key).and_then(Value::as_u64).unwrap_or(0) as usize;
        let length = item
            .get(&length_key)
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        return read_text_slice(path, offset, length);
    }
    item.get(inline_key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{inline_key} or {path_key} is required"))
}
fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        }
        else {
        	fs::copy(&source_path, &destination_path)
                .map(|_| ())
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}
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
    }
    else {
        usize::MAX
    };
    let truncated = total_bytes > cap_bytes;
    let encoded = if truncated {
        general_purpose::STANDARD.encode(&bytes[..cap_bytes])
    }
    else {
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
fn timestamp(value: Option<std::time::SystemTime>) -> Option<u64> {
    value
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}
fn bool_field(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}
fn usize_field(value: &Value, key: &str, default: usize) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default)
}
fn slice_chars(text: &str, offset: usize, length: Option<usize>) -> String {
    let start = char_byte_index(text, offset);
    let end = length
        .map(|length| start + char_byte_index(&text[start..], length))
        .unwrap_or(text.len());
    text[start..end].to_string()
}
fn char_byte_index(text: &str, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    text.char_indices()
        .nth(offset)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}
fn read_ascii_slice(
    path: &Path,
    offset: usize,
    length: Option<usize>,
) -> Result<SliceRead, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let mut buffer = [0u8; 64 * 1024];
    let mut content = String::with_capacity(length.unwrap_or(0).min(1024 * 1024));
    let mut byte_index = 0usize;
    let limit = length
        .map(|length| offset.saturating_add(length))
        .unwrap_or(usize::MAX);
    let mut line_breaks = 0usize;
    let mut saw_text = false;
    let mut last_was_lf = false;
    // On null-byte detection do not fs::read the file again; merge the accumulated chunks
    // and the remaining chunks straight into a binary buffer to skip a second disk read.
    let mut binary_buf: Option<Vec<u8>> = None;

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];

        if let Some(buf) = binary_buf.as_mut() {
            buf.extend_from_slice(chunk);
            continue;
        }
        // Combine the ASCII check and null-byte detection into a single pass.
        let mut null_at: Option<usize> = None;
        let mut non_ascii = false;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte == 0 {
                null_at = Some(index);
                break;
            }
            if *byte >= 0x80 {
                non_ascii = true;
                break;
            }
        }
        if non_ascii {
            return Ok(SliceRead::NonAscii);
        }
        if null_at.is_some() {
            let mut buf = Vec::with_capacity(byte_index + read);
            buf.extend_from_slice(chunk);
            binary_buf = Some(buf);
            continue;
        }
        saw_text = true;
        last_was_lf = chunk.last() == Some(&b'\n');
        line_breaks += chunk.iter().filter(|byte| **byte == b'\n').count();

        let chunk_start = byte_index;
        let chunk_end = byte_index + read;
        if chunk_end > offset && chunk_start < limit {
            let start = offset.saturating_sub(chunk_start);
            let end = (limit.min(chunk_end)) - chunk_start;
            let text = std::str::from_utf8(&chunk[start..end])
                .map_err(|error| format!("Failed to decode {}: {error}", path.display()))?;
            content.push_str(text);
        }
        byte_index = chunk_end;
    }
    if let Some(buf) = binary_buf {
        return Ok(SliceRead::Binary(buf));
    }
    let line_count = line_breaks + usize::from(saw_text && !last_was_lf);
    Ok(SliceRead::Text {
        content,
        line_count,
        byte_size: byte_index as u64,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn edit_lines_replaces_single_line_crlf_preserved() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-replace");

        let path = dir.join("sample.txt");
        let initial = (1..=20)
            .map(|index| format!("line {index}\r\n"))
            .collect::<String>();
        std::fs::write(&path, initial).unwrap();

        let result = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 15,
            "end_line": 15,
            "replacement": "line 15 REPLACED"
        }));
        assert!(!result.is_error, "{result:?}");
        let edited = std::fs::read_to_string(&path).unwrap();
        assert!(edited.contains("line 14\r\nline 15 REPLACED\r\nline 16\r\n"));
        assert!(!edited.contains("line 15\r\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_deletes_range() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-delete");

        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();

        let result = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 3,
            "end_line": 4,
            "replacement": ""
        }));
        assert!(!result.is_error, "{result:?}");
        let edited = std::fs::read_to_string(&path).unwrap();
        assert_eq!(edited, "a\nb\ne\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_inserts_after_line() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-insert");

        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();

        let result = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 2,
            "end_line": 2,
            "replacement": "INSERTED",
            "after": true
        }));
        assert!(!result.is_error, "{result:?}");
        let edited = std::fs::read_to_string(&path).unwrap();
        assert_eq!(edited, "a\nb\nINSERTED\nc\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_rejects_out_of_range() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-oor");

        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let result = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 99,
            "replacement": "X"
        }));
        assert!(result.is_error, "{result:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_rejects_empty_old_string() {
        let dir = make_temp_dir("rust-fs-mcp-edit-empty-old");

        let path = dir.join("sample.txt");
        std::fs::write(&path, "ab").unwrap();
        let result = edit_item(&json!({
            "file_path": path.display().to_string(),
            "old_string": "",
            "new_string": "x"
        }));
        assert!(result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ab");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_accepts_range_length_expected_lines() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-rangelen");

        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
        let result = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 2,
            "end_line": 4,
            "expected_lines": 3,
            "replacement": "X"
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nX\ne\n");
        // 전체 줄 수도, 범위 길이도 아니면 여전히 오류.
        let bad = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 1,
            "end_line": 1,
            "expected_lines": 99,
            "replacement": "Y"
        }));
        assert!(bad.is_error, "{bad:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn atomic_write_leaves_no_temp_files() {
        let dir = make_temp_dir("rust-fs-mcp-atomic-write");
        let path = dir.join("sample.txt");
        std::fs::write(&path, "old").unwrap();
        let result = handle_file_write(&json!({
            "items": [{ "path": path.display().to_string(), "content": "new content" }]
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn copy_then_remove_moves_file_and_dir() {
        let dir = make_temp_dir("rust-fs-mcp-xdev-move");
        let src_file = dir.join("a.txt");
        std::fs::write(&src_file, "payload").unwrap();
        let dst_file = dir.join("b.txt");
        copy_then_remove(&src_file, &dst_file).unwrap();
        assert!(!src_file.exists());
        assert_eq!(std::fs::read_to_string(&dst_file).unwrap(), "payload");
        let src_dir = dir.join("srcdir");
        std::fs::create_dir_all(src_dir.join("inner")).unwrap();
        std::fs::write(src_dir.join("inner").join("f.txt"), "x").unwrap();
        let dst_dir = dir.join("dstdir");
        copy_then_remove(&src_dir, &dst_dir).unwrap();
        assert!(!src_dir.exists());
        assert_eq!(
            std::fs::read_to_string(dst_dir.join("inner").join("f.txt")).unwrap(),
            "x"
        );
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
    fn dir_list_truncated_only_when_entries_exceed_max() {
        let dir = make_temp_dir("rust-fs-mcp-dirlist-trunc");
        for index in 0..3 {
            std::fs::write(dir.join(format!("f{index}.txt")), "x").unwrap();
        }
        let exact = list_dir_item(
            &json!({ "path": dir.display().to_string(), "maxEntries": 3 }),
            false,
        );
        let exact_meta = exact.structured.as_ref().unwrap();
        assert_eq!(exact_meta["truncated"], false, "{exact_meta:?}");
        assert_eq!(exact_meta["entryCount"], 3);
        let capped = list_dir_item(
            &json!({ "path": dir.display().to_string(), "maxEntries": 2 }),
            false,
        );
        let capped_meta = capped.structured.as_ref().unwrap();
        assert_eq!(capped_meta["truncated"], true, "{capped_meta:?}");
        assert_eq!(capped_meta["entryCount"], 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn large_ascii_read_streams_capped_with_full_metadata() {
        let dir = make_temp_dir("rust-fs-mcp-large-read");
        let path = dir.join("big.log");
        // 100 bytes per line * 3000 lines = 300KB, past the 2 * 80_000 streaming gate.
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
        assert_eq!(structured["truncated"], true);
        assert_eq!(structured["returnedChars"], 80_000);
        assert_eq!(structured["totalChars"], 300_000);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_counts_multiple_replacements_single_pass() {
        let dir = make_temp_dir("rust-fs-mcp-edit-multi");
        let path = dir.join("sample.txt");
        std::fs::write(&path, "aa bb aa cc aa").unwrap();
        let result = edit_item(&json!({
            "file_path": path.display().to_string(),
            "old_string": "aa",
            "new_string": "ZZ",
            "expected_replacements": 3
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ZZ bb ZZ cc ZZ");
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
    fn edit_lines_preserves_missing_trailing_newline() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-notrail");

        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb").unwrap();
        let result = edit_lines_item(&json!({
            "file_path": path.display().to_string(),
            "start_line": 2,
            "end_line": 2,
            "replacement": "B"
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nB");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_replacement_path_honors_offset_length() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-replpath");

        let target = dir.join("sample.txt");
        std::fs::write(&target, "a\nb\nc\n").unwrap();
        let source = dir.join("repl.txt");
        std::fs::write(&source, "XXXBYYY").unwrap();
        let result = edit_lines_item(&json!({
            "file_path": target.display().to_string(),
            "start_line": 2,
            "end_line": 2,
            "replacement_path": source.display().to_string(),
            "replacement_offset": 3,
            "replacement_length": 1
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "a\nB\nc\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn edit_lines_replacement_path_no_length_not_duplicated() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-nodup");

        let target = dir.join("sample.txt");
        std::fs::write(&target, "a\nb\nc\n").unwrap();
        let source = dir.join("repl.txt");
        std::fs::write(&source, "HELLO").unwrap();
        let result = edit_lines_item(&json!({
            "file_path": target.display().to_string(),
            "start_line": 2,
            "end_line": 2,
            "replacement_path": source.display().to_string()
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "a\nHELLO\nc\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    fn make_temp_dir(prefix: &str) -> std::path::PathBuf {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "{prefix}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    #[test]
    fn file_edit_matches_across_crlf_lf_mismatch() {

        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-edit-eol-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt");
        std::fs::write(&path, "line 19\r\nline 20\r\nline 21\r\n").unwrap();

        let result = edit_item(&json!({
            "file_path": path.display().to_string(),
            "old_string": "line 20\n",
            "new_string": "",
            "expected_replacements": 1
        }));
        assert!(!result.is_error, "{result:?}");
        let edited = std::fs::read_to_string(&path).unwrap();
        assert_eq!(edited, "line 19\r\nline 21\r\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn writes_and_reads_file() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-test-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));

        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt");
        let write = handle_file_write(&json!({
            "items": [{ "path": path.display().to_string(), "content": "abc" }]
        }));
        assert!(!write.is_error, "{write:?}");

        let read = handle_file_read(&json!({ "paths": [path.display().to_string()] }));
        assert!(!read.is_error);
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn dir_list_excludes_with_native_backend() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-dir-list-test-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));

        fs::create_dir_all(dir.join("target")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("target").join("skip.txt"), "skip").unwrap();
        fs::write(dir.join("src").join("keep.txt"), "keep").unwrap();

        let result = handle_dir_list(&json!({
            "items": [{
                "path": dir.display().to_string(),
                "depth": 2,
                "includeFiles": true,
                "excludePatterns": ["target"]
            }]
        }));
        assert!(!result.is_error, "{result:?}");
        let structured = result.structured.unwrap();
        // Compact batch entries are {index, ok, data}; the entry list itself lives in text.
        let inner = &structured["results"][0]["data"];
        assert_eq!(inner["backend"], "native-rust");
        assert_eq!(inner["entryCount"], 2);
        let text = result.content[0]["text"].as_str().unwrap();
        assert!(text.contains("src/"));
        assert!(text.contains("src/keep.txt"));
        assert!(!text.contains("target/"));

        fs::remove_dir_all(&dir).unwrap();
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
