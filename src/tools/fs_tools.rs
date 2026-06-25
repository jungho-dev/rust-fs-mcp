//! fs_tools.rs
//! tools::fs_tools
//!
//! Collection of file / directory / metadata / image / HTTP read handlers.
//! Exposes the two edit variants file-edit (exact block replace) and file-edit-lines (1-based line range replace) together.
//!

use crate::core::args_ref::read_text_slice;
use crate::core::batch::{create_batch_response, run_batch, run_batch_parallel};
use crate::core::config::{ensure_path_allowed, existing_path, target_path};
use crate::core::response::RawResult;
use base64::{Engine as _, engine::general_purpose};
use serde_json::{Map, Value, json};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
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
        return RawResult::error("paths or items is required");
    }

    let results = run_batch_parallel(&items, |item| read_item(item, allow_missing));
    create_batch_response("file-read", results, true)
}

pub fn handle_file_read_line_range(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let items = read_items(args);
    if items.is_empty() {
        return RawResult::error("paths or items is required");
    }

    let results = run_batch_parallel(&items, |item| lines_item(item, allow_missing));
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

// Default-on read cap: a whole-file read past this many characters is truncated unless
// RUST_FS_MCP_READ_MAX_CHARS overrides it. 0 disables the cap and restores full reads.
static READ_MAX_CHARS: OnceLock<usize> = OnceLock::new();
fn read_max_chars() -> usize {
    *READ_MAX_CHARS.get_or_init(|| {
        std::env::var("RUST_FS_MCP_READ_MAX_CHARS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(100_000)
    })
}

fn read_item(item: &Value, allow_missing: bool) -> RawResult {
    if bool_field(item, "isUrl", false) {
        return read_url_item(item);
    }

    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if !path.exists() {
        if allow_missing {
            return RawResult::structured(
                format!("Missing path: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    }

    if path.is_dir() {
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
    } else if text.ends_with('\n') {
        lf_count
    } else {
        lf_count + 1
    };

    // A whole-file read (no explicit length) past read_max_chars is capped so the envelope stays
    // within client token limits; an explicit length is always honored exactly as requested.
    // chars().count() <= len(), so a byte-length pre-check skips the full char scan for files
    // that cannot exceed the cap — the common case.
    let max_chars = read_max_chars();
    let maybe_over_cap =
        length.is_none() && max_chars > 0 && text.len().saturating_sub(offset) > max_chars;
    let total_chars = if maybe_over_cap {
        text.chars().count()
    } else {
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
    } else {
        format!("{}:\n{}", path.display(), sliced)
    };

    RawResult::structured(body, structured)
}

fn read_url_item(item: &Value) -> RawResult {
    let Some(url) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a URL string");
    };

    let response = match fetch_http_url(url, 0) {
        Ok(response) => response,
        Err(error) => return RawResult::error(error),
    };
    let content = String::from_utf8_lossy(&response.body).to_string();
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
            "status": response.status,
            "headers": response.headers,
            "bytes": response.body.len()
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
        let entry_path = entry.path();
        let marker = if entry_path.is_dir() { "/" } else { "" };
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
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if !path.exists() {
        if allow_missing {
            return RawResult::structured(
                format!("Missing path: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    }
    if path.is_dir() {
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
    let selected = match read_lines_native(&path, start_line, line_count) {
        Ok(selected) => selected,
        Err(error) => {
            return RawResult::error(error);
        }
    };

    lines_result(&path, selected, Some("native-rust"))
}

fn lines_result(path: &Path, selected: Vec<(usize, String)>, backend: Option<&str>) -> RawResult {
    let numbered = selected
        .iter()
        .map(|(number, line)| format!("{number}: {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    // The numbered body ships in text once; the previous structured.lines array re-sent every
    // line wrapped in {number,text} objects, more than doubling the payload.
    let mut structured = json!({
        "path": path.display().to_string(),
        "returned": selected.len()
    });
    if let Some(backend) = backend {
        structured["backend"] = json!(backend);
    }

    let mut result =
        RawResult::structured(format!("{}:\n{}", path.display(), numbered), structured);
    if let Some(backend) = backend {
        result.meta.insert("backend".to_string(), json!(backend));
    }

    result
}

fn read_lines_native(
    path: &Path,
    start_line: usize,
    line_count: Option<usize>,
) -> Result<Vec<(usize, String)>, String> {
    let file = fs::File::open(path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut selected = Vec::new();
    let mut line_number = 0usize;
    let mut skipped = Vec::new();
    while line_number + 1 < start_line {
        skipped.clear();
        let read = reader
            .read_until(b'\n', &mut skipped)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if read == 0 {
            return Ok(selected);
        }
        line_number += 1;
    }

    let mut line = String::new();
    loop {
        if let Some(line_count) = line_count
            && selected.len() >= line_count
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
        let index = line_number;
        selected.push((index + 1, std::mem::take(&mut line)));
        line_number += 1;
    }
    Ok(selected)
}

// 2. Write and directory tools ------------------------------------------------
pub fn handle_file_write(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items, write_item);
    create_batch_response("file-write", results, false)
}

pub fn handle_dir_create(args: &Value) -> RawResult {
    let Some(paths) = args.get("paths").and_then(Value::as_array) else {
        return RawResult::error("paths must be an array");
    };

    let items = paths
        .iter()
        .filter_map(Value::as_str)
        .map(|path| json!({ "path": path }))
        .collect::<Vec<_>>();
    let results = run_batch(&items, mkdir_item);
    create_batch_response("dir-create", results, false)
}

pub fn handle_dir_list(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch_parallel(items, |item| list_dir_item(item, allow_missing));
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
    } else {
        fs::write(&path, content.as_bytes())
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
    if !path.exists() {
        if allow_missing {
            return RawResult::structured(
                format!("Missing directory: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        return RawResult::error(format!("Directory does not exist: {}", path.display()));
    }
    if !path.is_dir() {
        return RawResult::error(format!("Path is not a directory: {}", path.display()));
    }

    let depth = usize_field(item, "depth", 2);
    let max_entries = usize_field(item, "maxEntries", usize::MAX);
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
        return dir_list_result(&path, Vec::new(), max_entries, None);
    }

    let (entries, backend) =
        match list_dir_native(&path, depth, max_entries, include_files, &excludes) {
            Ok(entries) => (entries, Some("native-rust")),
            Err(error) => return RawResult::error(error),
        };

    dir_list_result(&path, entries, max_entries, backend)
}

fn dir_list_result(
    path: &Path,
    entries: Vec<String>,
    max_entries: usize,
    backend: Option<&str>,
) -> RawResult {
    let truncated = entries.len() >= max_entries;
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
        .map_err(|error| format!("Failed to list {}: {error}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
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
        } else if ctx.include_files && is_file {
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
        if let Some(rest) = &self.rest
            && (rest.text == name || rest.glob.matches(name) || rest.glob.matches(rel))
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
        if let Some(bytes) = &self.pattern_bytes
            && value.is_ascii()
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
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = Some(pi);
            star_vi = vi;
            pi += 1;
        } else if let Some(index) = star_pi {
            pi = index + 1;
            star_vi += 1;
            vi = star_vi;
        } else {
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
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star_pi = Some(pi);
            star_vi = vi;
            pi += 1;
        } else if let Some(index) = star_pi {
            pi = index + 1;
            star_vi += 1;
            vi = star_vi;
        } else {
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
    } else {
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
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items, copy_item);
    create_batch_response("path-copy", results, false)
}

pub fn handle_path_move(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items, move_item);
    create_batch_response("path-move", results, false)
}

pub fn handle_path_remove(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items, remove_item);
    create_batch_response("path-remove", results, false)
}

pub fn handle_path_stat(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let Some(paths) = args.get("paths").and_then(Value::as_array) else {
        return RawResult::error("paths must be an array");
    };

    let items = paths
        .iter()
        .filter_map(Value::as_str)
        .map(|path| json!({ "path": path }))
        .collect::<Vec<_>>();
    let results = run_batch_parallel(&items, |item| info_item(item, allow_missing));
    create_batch_response("path-stat", results, false)
}

pub fn handle_file_edit(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items, edit_item);
    create_batch_response("file-edit", results, false)
}

pub fn handle_file_edit_lines(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items, edit_lines_item);
    create_batch_response("file-edit-lines", results, false)
}

fn copy_item(item: &Value) -> RawResult {
    let Some(source) = item.get("source").and_then(Value::as_str) else {
        return RawResult::error("source must be a string");
    };
    let Some(destination) = item.get("destination").and_then(Value::as_str) else {
        return RawResult::error("destination must be a string");
    };

    let source = match existing_path(source) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
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

    let result = if source.is_dir() {
        if !recursive {
            return RawResult::error("recursive must be true to copy a directory");
        }
        copy_dir_recursive(&source, &destination)
    } else {
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

    let source = match existing_path(source) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let destination = match target_path(destination) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if let Err(error) = ensure_parent_dir(&destination) {
        return RawResult::error(error);
    }

    match fs::rename(&source, &destination) {
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
    if !path.exists() {
        if force {
            return RawResult::structured(
                format!("Already absent: {}", path.display()),
                json!({ "path": path.display().to_string(), "removed": false }),
            );
        }
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    }

    let result = if path.is_dir() {
        if !bool_field(item, "recursive", false) {
            return RawResult::error("recursive must be true to remove a directory");
        }
        fs::remove_dir_all(&path)
    } else {
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

    if !path.exists() {
        if allow_missing {
            return RawResult::structured(
                format!("Missing path: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    }

    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return RawResult::error(format!("Failed to stat {}: {error}", path.display()));
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
    let path = match existing_path(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if path.is_dir() {
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

    let (effective_old, effective_new, eol_mode) =
        resolve_edit_strings(&text, &old_string, &new_string);
    let count = text.matches(&effective_old).count();
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

    let edited = text.replace(&effective_old, &effective_new);
    if let Err(error) = fs::write(&path, edited.as_bytes()) {
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
    let path = match existing_path(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if path.is_dir() {
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

    if let Some(expected) = item.get("expected_lines").and_then(Value::as_u64)
        && total_lines as u64 != expected
    {
        return RawResult::error(format!(
            "Expected {expected} lines but file has {total_lines}"
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
    let final_replacement = if trailing_eol_needed && (after || end_idx < total_lines) {
        let mut value = normalized;
        value.push_str(eol);
        value
    } else {
        normalized
    };

    let mut new_text = String::with_capacity(text.len() + final_replacement.len());
    if after {
        let insert_byte = if total_lines == 0 {
            0
        } else if end_idx < total_lines {
            line_ranges[end_idx].1
        } else {
            text.len()
        };
        new_text.push_str(&text[..insert_byte]);
        let needs_eol_before = insert_byte > 0
            && !text[..insert_byte].ends_with('\n')
            && !final_replacement.is_empty();
        if needs_eol_before {
            new_text.push_str(eol);
        }
        new_text.push_str(&final_replacement);
        new_text.push_str(&text[insert_byte..]);
    } else {
        let cut_start = line_ranges[start_idx].0;
        let cut_end = line_ranges[end_idx].1;
        new_text.push_str(&text[..cut_start]);
        new_text.push_str(&final_replacement);
        new_text.push_str(&text[cut_end..]);
    }

    if let Err(error) = fs::write(&path, new_text.as_bytes()) {
        return RawResult::error(format!("Failed to write {}: {error}", path.display()));
    }

    let action = if after {
        "insert_after"
    } else if final_replacement.is_empty() {
        "delete"
    } else {
        "replace"
    };
    let lines_changed = if after {
        0
    } else {
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
        } else {
            lf_only += 1;
        }
    }
    if crlf >= lf_only && crlf > 0 {
        "\r\n"
    } else {
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
            } else {
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
            } else {
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
        } else {
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
        } else {
            out.push(ch);
        }
    }
    out
}

// 4. Shared helpers -----------------------------------------------------------
fn ensure_parent_dir(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create {}: {error}", parent.display()))
}

fn item_content(item: &Value, inline_key: &str, path_key: &str) -> Result<String, String> {
    if let Some(path) = item.get(path_key).and_then(Value::as_str) {
        let path = ensure_path_allowed(path)?;
        let offset_key = inline_key
            .replace("string", "string_offset")
            .replace("content", "content_offset");
        let length_key = inline_key
            .replace("string", "string_length")
            .replace("content", "content_length");
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
        } else {
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
        Some("bmp") => Some("image/bmp"),
        Some("svg") => Some("image/svg+xml"),
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

struct HttpResponse {
    status: u16,
    headers: Vec<String>,
    body: Vec<u8>,
}

fn fetch_http_url(url: &str, redirects: usize) -> Result<HttpResponse, String> {
    if redirects > 5 {
        return Err("Too many HTTP redirects".to_string());
    }
    let parsed = parse_http_url(url)?;
    let mut stream = TcpStream::connect((&*parsed.host, parsed.port))
        .map_err(|error| format!("Failed to connect to {}: {error}", parsed.host))?;
    let timeout = Some(Duration::from_secs(20));
    stream
        .set_read_timeout(timeout)
        .map_err(|error| format!("Failed to set read timeout: {error}"))?;
    stream
        .set_write_timeout(timeout)
        .map_err(|error| format!("Failed to set write timeout: {error}"))?;

    let host_header = if parsed.port == 80 {
        parsed.host.clone()
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: rust-fs-mcp/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        parsed.path,
        host_header,
        env!("CARGO_PKG_VERSION")
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("Failed to write HTTP request: {error}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| format!("Failed to read HTTP response: {error}"))?;
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "Invalid HTTP response: missing header terminator".to_string())?;
    let header_text = String::from_utf8_lossy(&response[..header_end]);
    let mut header_lines = header_text.lines();
    let status_line = header_lines
        .next()
        .ok_or_else(|| "Invalid HTTP response: missing status line".to_string())?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| format!("Invalid HTTP status line: {status_line}"))?;
    let headers = header_lines.map(str::to_string).collect::<Vec<_>>();
    let location = if matches!(status, 301 | 302 | 303 | 307 | 308) {
        redirect_location(&headers)
    } else {
        None
    };
    if let Some(location) = location {
        let next_url = if location.starts_with("http://") || location.starts_with("https://") {
            location
        } else {
            format!("http://{}{}", parsed.host, location)
        };
        return fetch_http_url(&next_url, redirects + 1);
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP request failed with status {status}"));
    }

    Ok(HttpResponse {
        status,
        headers,
        body: response[header_end + 4..].to_vec(),
    })
}

struct ParsedHttpUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Result<ParsedHttpUrl, String> {
    if url.starts_with("https://") {
        return Err("HTTPS URL reads require a TLS-capable Rust HTTP client layer".to_string());
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return Err("Only http:// and https:// URL schemes are accepted".to_string());
    };
    let (authority, path) = rest
        .split_once('/')
        .map(|(authority, path)| (authority, format!("/{path}")))
        .unwrap_or((rest, "/".to_string()));
    if authority.is_empty() {
        return Err("HTTP URL host is required".to_string());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|ch| ch.is_ascii_digit()) => {
            let port = port
                .parse::<u16>()
                .map_err(|error| format!("Invalid HTTP port: {error}"))?;
            (host.to_string(), port)
        }
        _ => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err("HTTP URL host is required".to_string());
    }

    Ok(ParsedHttpUrl { host, port, path })
}

fn redirect_location(headers: &[String]) -> Option<String> {
    headers.iter().find_map(|header| {
        let (name, value) = header.split_once(':')?;
        if name.eq_ignore_ascii_case("location") {
            Some(value.trim().to_string())
        } else {
            None
        }
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
        let _guard = edit_lines_lock();
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
        let _guard = edit_lines_lock();
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
        let _guard = edit_lines_lock();
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
        let _guard = edit_lines_lock();
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

    fn config_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::Mutex;
        use std::sync::OnceLock;
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    fn edit_lines_lock() -> std::sync::MutexGuard<'static, ()> {
        let guard = config_lock();
        let target = std::env::current_dir().unwrap();
        crate::core::config::handle_set_config_values(&json!({
            "items": [{
                "key": "allowedDirectories",
                "value": [target.display().to_string()]
            }]
        }));
        guard
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
        let _guard = config_lock();
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
        let project = std::env::current_dir().unwrap();
        crate::core::config::handle_set_config_values(&json!({
            "items": [{
                "key": "allowedDirectories",
                "value": [project.display().to_string()]
            }]
        }));
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
        let _guard = config_lock();
        let project = std::env::current_dir().unwrap();
        crate::core::config::handle_set_config_values(&json!({
            "items": [{
                "key": "allowedDirectories",
                "value": [project.display().to_string()]
            }]
        }));
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
        let _guard = config_lock();
        let project = std::env::current_dir().unwrap();
        crate::core::config::handle_set_config_values(&json!({
            "items": [{
                "key": "allowedDirectories",
                "value": [project.display().to_string()]
            }]
        }));
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

        let read = handle_file_read(&json!({
            "items": [{ "path": format!("http://{addr}/"), "isUrl": true }]
        }));
        assert!(!read.is_error, "{read:?}");
    }
}
