//! inspect_tools.rs
//! tools::inspect_tools
//!
//! Compact read-only filesystem inspection (fs-inspect) tool aimed at coding workflows.
//! Bundles count-files / search / json-pick / snippet modes into one batched call.
//!

use crate::core::config::ensure_path_allowed;
use crate::core::response::RawResult;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

// inspect_tools' wildcard_match applies the same pattern to many entries repeatedly.
// Re-running Regex::new on every call would blow up compile cost and allocations, so
// compiled Regex values are cached per pattern. Mirrors search_tools' GLOB_CACHE pattern.
static INSPECT_WILDCARD_CACHE: OnceLock<RwLock<HashMap<String, Regex>>> = OnceLock::new();

struct InspectState {
    max_chars: usize,
    used_chars: usize,
    scanned_files: usize,
    bytes_read: usize,
    truncated: bool,
}

struct Hit {
    rel: String,
    line: usize,
    text: String,
    fields: Map<String, Value>,
}

struct ExtractSpec {
    name: String,
    regex: Regex,
}

struct SearchCtx<'a> {
    root: &'a Path,
    recursive: bool,
    file_pattern: Option<&'a str>,
    matcher: &'a Regex,
    extracts: &'a [ExtractSpec],
    max_matches: usize,
}

// 1. FS inspect tool ----------------------------------------------------------
pub fn handle_fs_inspect(args: &Value) -> RawResult {
    let Some(root_text) = args.get("root").and_then(Value::as_str) else {
        return RawResult::error("root must be a string");
    };
    let Some(requests) = args.get("requests").and_then(Value::as_array) else {
        return RawResult::error("requests must be an array");
    };

    let root = match inspect_root(root_text) {
        Ok(root) => root,
        Err(error) => return RawResult::error(error),
    };
    let max_chars = args
        .get("maxSnippetChars")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or_else(|| usize_field(args, "maxEvidenceChars", 6000));
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("strict");
    let mut state = InspectState {
        max_chars,
        used_chars: 0,
        scanned_files: 0,
        bytes_read: 0,
        truncated: false,
    };
    let answers = requests
        .iter()
        .enumerate()
        .map(|(index, request)| run_request(&root, request, index + 1, &mut state))
        .collect::<Vec<_>>();
    let status = inspect_status(&answers);
    let text = format!(
        "fs-inspect: {} requests, status={status}, snippetChars={}",
        answers.len(),
        state.used_chars
    );

    RawResult::structured(
        text,
        json!({
            "status": status,
            "mode": mode,
            "answers": answers,
            "metrics": {
                "scannedFiles": state.scanned_files,
                "bytesRead": state.bytes_read,
                "snippetChars": state.used_chars,
                "truncated": state.truncated
            }
        }),
    )
}

// 2. Request dispatch ---------------------------------------------------------
fn run_request(root: &Path, request: &Value, index: usize, state: &mut InspectState) -> Value {
    let id = request
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("request-{index}"));
    let op = request
        .get("op")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    match op {
        "count-files" => count_files(root, request, &id, state),
        "search" => search_files(root, request, &id, state),
        "json-pick" => json_pick(root, request, &id, state),
        "snippet" => snippets(root, request, &id, state),
        "git-status" => git_status_answer(root, request, &id),
        _ => answer_error(&id, op, format!("Unsupported op: {op}")),
    }
}

// 2b. Git status inspection ---------------------------------------------------
// Composite op: folds a git status/branch lookup into the same fs-inspect call,
// so read + search + git resolve in ONE tool round-trip instead of three.
fn git_status_answer(root: &Path, request: &Value, id: &str) -> Value {
    let path = request
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| root.display().to_string());
    let result = crate::tools::git_tools::handle_git_status(&json!({ "path": path }));
    let text = result
        .content
        .first()
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if result.is_error {
        let message = if text.is_empty() {
            "git-status failed".to_string()
        } else {
            text
        };
        return answer_error(id, "git-status", message);
    }
    // git-status keeps its porcelain body in content text; fold it into the answer value here.
    let repo_path = result
        .structured
        .as_ref()
        .and_then(|structured| structured.get("path"))
        .cloned()
        .unwrap_or(Value::Null);
    answer_ok(
        id,
        "git-status",
        json!({ "path": repo_path, "status": text }),
        "high",
        Vec::new(),
        Vec::new(),
    )
}

// 3. Count files --------------------------------------------------------------
fn count_files(root: &Path, request: &Value, id: &str, state: &mut InspectState) -> Value {
    let op = "count-files";
    let path = match request_path(root, request) {
        Ok(path) => path,
        Err(error) => return answer_error(id, op, error),
    };
    if !path.is_dir() {
        return answer_error(
            id,
            op,
            format!("Path is not a directory: {}", path.display()),
        );
    }

    let glob = request
        .get("glob")
        .or_else(|| request.get("pattern"))
        .and_then(Value::as_str)
        .unwrap_or("*");
    let recursive = bool_field(request, "recursive", false);
    let mut samples = Vec::new();
    let count = match count_dir(root, &path, glob, recursive, &mut samples, state) {
        Ok(count) => count,
        Err(error) => return answer_error(id, op, error),
    };
    let mut evidence = Vec::new();
    let sample_text = if samples.is_empty() {
        "no matched files".to_string()
    } else {
        format!("sample: {}", samples.join(", "))
    };
    add_evidence(
        &mut evidence,
        rel_path(root, &path),
        None,
        None,
        sample_text,
        state,
    );

    answer_ok(
        id,
        op,
        json!({
            "path": rel_path(root, &path),
            "glob": glob,
            "recursive": recursive,
            "count": count
        }),
        "high",
        evidence,
        Vec::new(),
    )
}

// 4. Search files -------------------------------------------------------------
fn search_files(root: &Path, request: &Value, id: &str, state: &mut InspectState) -> Value {
    let op = "search";
    let path = match request_path(root, request) {
        Ok(path) => path,
        Err(error) => return answer_error(id, op, error),
    };
    let Some(pattern) = request.get("pattern").and_then(Value::as_str) else {
        return answer_error(id, op, "pattern must be a string");
    };

    let literal = bool_field(request, "literal", false);
    let recursive = bool_field(request, "recursive", true);
    let max_matches = usize_field(request, "maxMatches", 20);
    let file_pattern = request.get("filePattern").and_then(Value::as_str);
    let matcher = match search_regex(pattern, literal) {
        Ok(regex) => regex,
        Err(error) => return answer_error(id, op, error),
    };
    let extracts = match extract_specs(request) {
        Ok(specs) => specs,
        Err(error) => return answer_error(id, op, error),
    };
    let mut hits = Vec::new();
    let mut warnings = Vec::new();
    let ctx = SearchCtx {
        root,
        recursive,
        file_pattern,
        matcher: &matcher,
        extracts: &extracts,
        max_matches,
    };

    if let Err(error) = search_path(&ctx, &path, &mut hits, &mut warnings, state) {
        return answer_error(id, op, error);
    }

    if hits.is_empty() {
        warnings.push("no matches".to_string());
        return answer_partial(id, op, json!({ "matches": 0 }), "low", Vec::new(), warnings);
    }

    let mut value = Map::new();
    value.insert("matches".to_string(), json!(hits.len()));
    value.insert("path".to_string(), json!(hits[0].rel.as_str()));
    for (key, value_item) in &hits[0].fields {
        value.insert(key.clone(), value_item.clone());
    }

    let mut evidence = Vec::new();
    for hit in &hits {
        add_evidence(
            &mut evidence,
            hit.rel.clone(),
            Some(hit.line),
            Some(hit.line),
            hit.text.clone(),
            state,
        );
    }
    if hits.len() > 1 {
        warnings.push("multiple matches".to_string());
    }
    let confidence = if hits.len() == 1 { "high" } else { "medium" };

    answer_ok(id, op, Value::Object(value), confidence, evidence, warnings)
}

// 5. JSON pointer picks -------------------------------------------------------
fn json_pick(root: &Path, request: &Value, id: &str, state: &mut InspectState) -> Value {
    let op = "json-pick";
    let path = match request_path(root, request) {
        Ok(path) => path,
        Err(error) => return answer_error(id, op, error),
    };
    let Some(pointers) = request.get("pointers").and_then(Value::as_array) else {
        return answer_error(id, op, "pointers must be an array");
    };

    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => return answer_error(id, op, format!("Failed to read file: {error}")),
    };
    state.scanned_files += 1;
    state.bytes_read += text.len();
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => return answer_error(id, op, format!("Invalid JSON: {error}")),
    };

    let mut values = Map::new();
    let mut warnings = Vec::new();
    let mut evidence = Vec::new();
    for pointer in pointers.iter().filter_map(Value::as_str) {
        if let Some(value) = parsed.pointer(pointer) {
            values.insert(pointer.to_string(), value.clone());
            add_evidence(
                &mut evidence,
                rel_path(root, &path),
                None,
                None,
                format!("{pointer} = {}", compact_json(value)),
                state,
            );
        } else {
            warnings.push(format!("missing pointer: {pointer}"));
        }
    }
    let status = if warnings.is_empty() { "ok" } else { "partial" };
    let confidence = if warnings.is_empty() {
        "high"
    } else {
        "medium"
    };

    answer(
        id,
        op,
        status,
        json!({ "path": rel_path(root, &path), "values": values }),
        confidence,
        evidence,
        warnings,
    )
}

// 6. Snippet collection -------------------------------------------------------
fn snippets(root: &Path, request: &Value, id: &str, state: &mut InspectState) -> Value {
    let op = "snippet";
    let path = match request_path(root, request) {
        Ok(path) => path,
        Err(error) => return answer_error(id, op, error),
    };
    let Some(patterns) = string_array(request, "patterns") else {
        return answer_error(id, op, "patterns must be a string array");
    };

    let context = usize_field(request, "contextLines", 2);
    let max_snips = usize_field(request, "maxSnippets", 10);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => return answer_error(id, op, format!("Failed to read file: {error}")),
    };
    state.scanned_files += 1;
    state.bytes_read += text.len();

    let lines = text.lines().collect::<Vec<_>>();
    let mut ranges = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !patterns.iter().any(|pattern| line.contains(pattern)) {
            continue;
        }
        let start = index.saturating_sub(context);
        let end = (index + context + 1).min(lines.len());
        push_range(&mut ranges, start, end);
        if ranges.len() >= max_snips {
            break;
        }
    }

    if ranges.is_empty() {
        return answer_partial(
            id,
            op,
            json!({ "path": rel_path(root, &path), "matches": 0 }),
            "low",
            Vec::new(),
            vec!["no snippets matched".to_string()],
        );
    }

    let mut evidence = Vec::new();
    for (start, end) in &ranges {
        let snippet = (*start..*end)
            .map(|index| format!("{}: {}", index + 1, lines[index]))
            .collect::<Vec<_>>()
            .join("\n");
        add_evidence(
            &mut evidence,
            rel_path(root, &path),
            Some(start + 1),
            Some(*end),
            snippet,
            state,
        );
    }

    answer_ok(
        id,
        op,
        json!({
            "path": rel_path(root, &path),
            "matches": ranges.len(),
            "ranges": ranges
                .into_iter()
                .map(|(start, end)| json!({ "lineStart": start + 1, "lineEnd": end }))
                .collect::<Vec<_>>()
        }),
        "high",
        evidence,
        Vec::new(),
    )
}

// 7. Root and request paths ---------------------------------------------------
fn inspect_root(root: &str) -> Result<PathBuf, String> {
    let root = ensure_path_allowed(root)?;
    if !root.is_dir() {
        return Err(format!("root is not a directory: {}", root.display()));
    }
    fs::canonicalize(&root).map_err(|error| format!("Failed to canonicalize root: {error}"))
}

fn request_path(root: &Path, request: &Value) -> Result<PathBuf, String> {
    let Some(path_text) = request.get("path").and_then(Value::as_str) else {
        return Err("path must be a string".to_string());
    };
    let raw = Path::new(path_text);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let path = ensure_path_allowed(joined)?;
    if !path.exists() {
        return Err(format!("Path does not exist: {}", path.display()));
    }
    let path = fs::canonicalize(&path)
        .map_err(|error| format!("Failed to canonicalize {}: {error}", path.display()))?;
    if !within_root(root, &path) {
        return Err(format!("Path is outside root: {}", path.display()));
    }

    Ok(path)
}

// 8. Directory traversal ------------------------------------------------------
fn count_dir(
    root: &Path,
    dir: &Path,
    glob: &str,
    recursive: bool,
    samples: &mut Vec<String>,
    state: &mut InspectState,
) -> Result<usize, String> {
    let mut count = 0;
    for entry in read_dir(dir)? {
        let path = entry.path();
        if path.is_dir() {
            if recursive {
                count += count_dir(root, &path, glob, recursive, samples, state)?;
            }
            continue;
        }
        if !path.is_file() {
            continue;
        }

        state.scanned_files += 1;
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if !wildcard_match(glob, name) {
            continue;
        }

        count += 1;
        if samples.len() < 20 {
            samples.push(rel_path(root, &path));
        }
    }

    Ok(count)
}

fn search_path(
    ctx: &SearchCtx<'_>,
    path: &Path,
    hits: &mut Vec<Hit>,
    warnings: &mut Vec<String>,
    state: &mut InspectState,
) -> Result<(), String> {
    if hits.len() >= ctx.max_matches {
        return Ok(());
    }
    if path.is_file() {
        search_file(ctx, path, hits, warnings, state);
        return Ok(());
    }
    if !path.is_dir() {
        return Err(format!(
            "Path is not a file or directory: {}",
            path.display()
        ));
    }

    let mut entries = read_dir(path)?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        if hits.len() >= ctx.max_matches {
            warnings.push("maxMatches reached".to_string());
            return Ok(());
        }
        let child = entry.path();
        if child.is_dir() {
            if ctx.recursive && child.file_name().and_then(|value| value.to_str()) != Some(".git") {
                search_path(ctx, &child, hits, warnings, state)?;
            }
        } else {
            search_file(ctx, &child, hits, warnings, state);
        }
    }

    Ok(())
}

fn search_file(
    ctx: &SearchCtx<'_>,
    path: &Path,
    hits: &mut Vec<Hit>,
    warnings: &mut Vec<String>,
    state: &mut InspectState,
) {
    if hits.len() >= ctx.max_matches || !path.is_file() {
        return;
    }
    let rel = rel_path(ctx.root, path);
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if let Some(pattern) = ctx.file_pattern
        && !wildcard_match(pattern, name)
        && !wildcard_match(pattern, &rel)
    {
        return;
    }

    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            warnings.push(format!("skipped {rel}: {error}"));
            return;
        }
    };
    state.scanned_files += 1;

    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut index = 0usize;
    loop {
        if hits.len() >= ctx.max_matches {
            return;
        }
        line.clear();
        let read = match reader.read_line(&mut line) {
            Ok(read) => read,
            Err(error) => {
                warnings.push(format!("skipped {rel}: {error}"));
                return;
            }
        };
        if read == 0 {
            break;
        }
        state.bytes_read += read;
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        if !ctx.matcher.is_match(&line) {
            index += 1;
            continue;
        }

        hits.push(Hit {
            rel: rel.clone(),
            line: index + 1,
            text: line.clone(),
            fields: capture_fields(&line, ctx.extracts),
        });
        index += 1;
    }
}

// 9. Search helpers -----------------------------------------------------------
fn search_regex(pattern: &str, literal: bool) -> Result<Regex, String> {
    let pattern = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    Regex::new(&pattern).map_err(|error| format!("Invalid search pattern: {error}"))
}

fn extract_specs(request: &Value) -> Result<Vec<ExtractSpec>, String> {
    let Some(items) = request.get("extract") else {
        return Ok(Vec::new());
    };
    let Some(items) = items.as_array() else {
        return Err("extract must be an array".to_string());
    };

    let mut specs = Vec::new();
    for item in items {
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            return Err("extract.name must be a string".to_string());
        };
        let Some(pattern) = item.get("regex").and_then(Value::as_str) else {
            return Err("extract.regex must be a string".to_string());
        };
        let regex = Regex::new(pattern)
            .map_err(|error| format!("Invalid extract regex for {name}: {error}"))?;
        specs.push(ExtractSpec {
            name: name.to_string(),
            regex,
        });
    }

    Ok(specs)
}

fn capture_fields(line: &str, specs: &[ExtractSpec]) -> Map<String, Value> {
    let mut fields = Map::new();
    for spec in specs {
        if let Some(captures) = spec.regex.captures(line)
            && let Some(value) = captures.get(1)
        {
            fields.insert(spec.name.clone(), json!(value.as_str()));
        }
    }

    fields
}

// 10. Evidence shaping --------------------------------------------------------
fn add_evidence(
    evidence: &mut Vec<Value>,
    path: String,
    line_start: Option<usize>,
    line_end: Option<usize>,
    snippet: String,
    state: &mut InspectState,
) {
    if state.used_chars >= state.max_chars {
        state.truncated = true;
        return;
    }

    let remaining = state.max_chars - state.used_chars;
    // For mostly-ASCII snippets avoid two chars().count() scans. When snippet.len() <= remaining
    // it is guaranteed that chars().count() <= len, so pass through without a chars check.
    let (snippet, snippet_chars) = if snippet.len() <= remaining {
        let count = snippet.chars().count();
        (snippet, count)
    } else {
        // Possible multi-byte content — count exactly and truncate if needed.
        let count = snippet.chars().count();
        if count > remaining {
            state.truncated = true;
            let truncated = truncate_chars(&snippet, remaining);
            let truncated_count = truncated.chars().count();
            (truncated, truncated_count)
        } else {
            (snippet, count)
        }
    };
    state.used_chars += snippet_chars;

    evidence.push(json!({
        "path": path,
        "lineStart": line_start,
        "lineEnd": line_end,
        "snippet": snippet
    }));
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

// 11. Answer builders ---------------------------------------------------------
fn answer_ok(
    id: &str,
    op: &str,
    value: Value,
    confidence: &str,
    evidence: Vec<Value>,
    warnings: Vec<String>,
) -> Value {
    answer(id, op, "ok", value, confidence, evidence, warnings)
}

fn answer_partial(
    id: &str,
    op: &str,
    value: Value,
    confidence: &str,
    evidence: Vec<Value>,
    warnings: Vec<String>,
) -> Value {
    answer(id, op, "partial", value, confidence, evidence, warnings)
}

fn answer_error(id: &str, op: &str, message: impl Into<String>) -> Value {
    answer(
        id,
        op,
        "error",
        Value::Null,
        "low",
        Vec::new(),
        vec![message.into()],
    )
}

fn answer(
    id: &str,
    op: &str,
    status: &str,
    value: Value,
    confidence: &str,
    evidence: Vec<Value>,
    warnings: Vec<String>,
) -> Value {
    json!({
        "id": id,
        "op": op,
        "status": status,
        "value": value,
        "confidence": confidence,
        "evidence": evidence,
        "warnings": warnings
    })
}

fn inspect_status(answers: &[Value]) -> &'static str {
    if answers
        .iter()
        .all(|answer| answer.get("status").and_then(Value::as_str) == Some("error"))
    {
        return "error";
    }
    let has_error = answers
        .iter()
        .any(|answer| answer.get("status").and_then(Value::as_str) == Some("error"));
    let has_partial = answers
        .iter()
        .any(|answer| answer.get("status").and_then(Value::as_str) == Some("partial"));
    if has_error || has_partial {
        "partial"
    } else {
        "ok"
    }
}

// 12. Small helpers -----------------------------------------------------------
fn read_dir(path: &Path) -> Result<Vec<fs::DirEntry>, String> {
    let mut entries = fs::read_dir(path)
        .map_err(|error| format!("Failed to list {}: {error}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Failed to read directory entry: {error}"))?;
    entries.sort_by_key(|entry| entry.path());
    Ok(entries)
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

fn string_array(value: &Value, key: &str) -> Option<Vec<String>> {
    value.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

fn push_range(ranges: &mut Vec<(usize, usize)>, start: usize, end: usize) {
    if let Some(last) = ranges.last_mut()
        && start <= last.1
    {
        last.1 = last.1.max(end);
        return;
    }
    ranges.push((start, end));
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn within_root(root: &Path, path: &Path) -> bool {
    let root = cmp_path(root);
    let path = cmp_path(path);
    path == root || path.starts_with(&format!("{root}/"))
}

fn cmp_path(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = value.strip_prefix("//?/") {
        value = stripped.to_string();
    }
    if cfg!(windows) {
        value = value.to_ascii_lowercase();
    }
    value.trim_end_matches('/').to_string()
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let cache = INSPECT_WILDCARD_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    // Regex is Sync so is_match runs directly under the read lock, removing the per-call Arc clone.
    if let Some(regex) = cache.read().unwrap().get(pattern) {
        return regex.is_match(text);
    }
    let mut source = String::with_capacity(pattern.len() * 2 + 2);
    source.push('^');
    for ch in pattern.chars() {
        match ch {
            '*' => source.push_str(".*"),
            '?' => source.push('.'),
            ch if matches!(
                ch,
                '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\' | '*' | '?'
            ) =>
            {
                source.push('\\');
                source.push(ch);
            }
            ch => source.push(ch),
        }
    }
    source.push('$');
    let Ok(regex) = Regex::new(&source) else {
        return false;
    };
    let mut cache_w = cache.write().unwrap();
    cache_w
        .entry(pattern.to_string())
        .or_insert(regex)
        .is_match(text)
}
