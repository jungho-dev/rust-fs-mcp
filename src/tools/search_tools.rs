use crate::core::args_ref::read_text_slice;
use crate::core::batch::{create_batch_response, run_batch, run_batch_parallel};
use crate::core::bundled::{BundledTool, run_bundled};
use crate::core::config::ensure_path_allowed;
use crate::core::response::RawResult;
use regex::RegexBuilder;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug)]
struct SearchSession {
    lines: Vec<String>,
    backend: String,
}

static SESSIONS: OnceLock<Mutex<HashMap<String, SearchSession>>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

// 1. Search tools -------------------------------------------------------------
pub fn handle_search_start(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items.clone(), start_item);
    create_batch_response("search-start", results, false)
}

pub fn handle_search_regex(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch_parallel(items.clone(), regex_item);
    create_batch_response("search-regex", results, true)
}

pub fn handle_search_get(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items.clone(), get_item);
    create_batch_response("search-get", results, true)
}

pub fn handle_search_stop(args: &Value) -> RawResult {
    let Some(session_ids) = args.get("sessionIds").and_then(Value::as_array) else {
        return RawResult::error("sessionIds must be an array");
    };

    let items = session_ids
        .iter()
        .filter_map(Value::as_str)
        .map(|session_id| json!({ "sessionId": session_id }))
        .collect();
    let results = run_batch(items, stop_item);
    create_batch_response("search-stop", results, false)
}

fn start_item(item: Value) -> RawResult {
    let search = match run_start_search(&item) {
        Ok(search) => search,
        Err(error) => return RawResult::error(error),
    };
    let session_id = format!("search-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let preview = search.lines.iter().take(20).cloned().collect::<Vec<_>>();
    let total = search.lines.len();
    let backend = search.backend.clone();
    sessions()
        .lock()
        .unwrap()
        .insert(session_id.clone(), search);

    RawResult::structured(
        format!("{session_id}: {total} results"),
        json!({
            "sessionId": session_id,
            "backend": backend,
            "totalCount": total,
            "preview": preview
        }),
    )
}

fn regex_item(item: Value) -> RawResult {
    let search = match run_regex_search(&item) {
        Ok(search) => search,
        Err(error) => return RawResult::error(error),
    };
    let text = search.lines.join("\n");
    let backend = search.backend.clone();
    let mut result = RawResult::structured(
        text.clone(),
        json!({
            "backend": backend.clone(),
            "totalCount": search.lines.len(),
            "results": search.lines,
            "text": text
        }),
    );
    result.meta.insert("backend".to_string(), json!(backend));
    result
}

fn get_item(item: Value) -> RawResult {
    let Some(session_id) = item.get("sessionId").and_then(Value::as_str) else {
        return RawResult::error("sessionId must be a string");
    };

    let sessions = sessions().lock().unwrap();
    let Some(session) = sessions.get(session_id) else {
        return RawResult::error(format!("Search session not found: {session_id}"));
    };

    let offset = item.get("offset").and_then(Value::as_i64).unwrap_or(0);
    let length = item
        .get("length")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(session.lines.len());
    let start = if offset < 0 {
        session
            .lines
            .len()
            .saturating_sub(offset.unsigned_abs() as usize)
    } else {
        offset as usize
    };
    let results = session
        .lines
        .iter()
        .skip(start)
        .take(length)
        .cloned()
        .collect::<Vec<_>>();
    let text = results.join("\n");

    RawResult::structured(
        text.clone(),
        json!({
            "sessionId": session_id,
            "backend": session.backend.as_str(),
            "offset": start,
            "length": results.len(),
            "totalCount": session.lines.len(),
            "results": results,
            "text": text
        }),
    )
}

fn stop_item(item: Value) -> RawResult {
    let Some(session_id) = item.get("sessionId").and_then(Value::as_str) else {
        return RawResult::error("sessionId must be a string");
    };

    let removed = sessions().lock().unwrap().remove(session_id).is_some();
    RawResult::structured(
        format!("Stopped {session_id}: {removed}"),
        json!({ "sessionId": session_id, "removed": removed }),
    )
}

// 2. Bundled search runner ----------------------------------------------------
fn run_start_search(item: &Value) -> Result<SearchSession, String> {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return Err("path must be a string".to_string());
    };
    let path = ensure_path_allowed(path)?;
    let pattern = read_pattern(item)?;
    let search_type = item
        .get("searchType")
        .and_then(Value::as_str)
        .unwrap_or("files");
    let include_hidden = bool_field(item, "includeHidden", false);
    let ignore_case = bool_field(item, "ignoreCase", true);
    let file_pattern = item.get("filePattern").and_then(Value::as_str);
    let max_results = item
        .get("maxResults")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(usize::MAX);

    if max_results == 0 {
        return Ok(SearchSession {
            lines: Vec::new(),
            backend: "bundled".to_string(),
        });
    }

    if search_type == "files" {
        return run_fd_search(
            &path,
            &pattern,
            ignore_case,
            include_hidden,
            file_pattern,
            max_results,
        );
    }

    run_rg_search(RgOpts {
        path: &path,
        pattern: &pattern,
        item,
        include_hidden,
        file_pattern,
        max_results,
        context_default: 5,
        allow_literal: true,
        timeout_default: None,
    })
}

fn run_regex_search(item: &Value) -> Result<SearchSession, String> {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return Err("path must be a string".to_string());
    };
    let path = ensure_path_allowed(path)?;
    let pattern = read_pattern(item)?;
    let include_hidden = bool_field(item, "includeHidden", false);
    let file_pattern = item.get("filePattern").and_then(Value::as_str);
    let max_results = item
        .get("maxResults")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(usize::MAX);
    if max_results == 0 {
        return Ok(SearchSession {
            lines: Vec::new(),
            backend: BundledTool::Rg.backend_name().to_string(),
        });
    }
    run_rg_search(RgOpts {
        path: &path,
        pattern: &pattern,
        item,
        include_hidden,
        file_pattern,
        max_results,
        context_default: 2,
        allow_literal: false,
        timeout_default: Some(10_000),
    })
}

struct RgOpts<'a> {
    path: &'a Path,
    pattern: &'a str,
    item: &'a Value,
    include_hidden: bool,
    file_pattern: Option<&'a str>,
    max_results: usize,
    context_default: usize,
    allow_literal: bool,
    timeout_default: Option<u64>,
}

fn run_fd_search(
    path: &Path,
    pattern: &str,
    ignore_case: bool,
    include_hidden: bool,
    file_pattern: Option<&str>,
    max_results: usize,
) -> Result<SearchSession, String> {
    let mut lines = Vec::new();
    let mut args = vec![
        "--color=never".to_string(),
        "--absolute-path".to_string(),
        "--no-ignore".to_string(),
        "--type".to_string(),
        "f".to_string(),
    ];
    if include_hidden {
        args.push("--hidden".to_string());
    }
    args.push(String::new());
    args.push(path.display().to_string());

    let output = run_bundled(BundledTool::Fd, &args, None, None)?;
    if output.status_code != Some(0) {
        return Err(format!(
            "bundled fd failed with code {:?}: {}",
            output.status_code,
            output.stderr.trim()
        ));
    }
    let patterns = split_patterns(file_pattern);
    for line in output.stdout.lines() {
        if file_search_match(path, line, pattern, &patterns, ignore_case) {
            lines.push(line.to_string());
        }
        if lines.len() >= max_results {
            break;
        }
    }

    Ok(SearchSession {
        lines,
        backend: BundledTool::Fd.backend_name().to_string(),
    })
}

fn run_rg_search(opts: RgOpts<'_>) -> Result<SearchSession, String> {
    let context = opts
        .item
        .get("contextLines")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(opts.context_default);
    let timeout_ms = opts
        .item
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .or(opts.timeout_default);
    let mut args = vec![
        "--json".to_string(),
        "--line-number".to_string(),
        "--color=never".to_string(),
        "--no-ignore".to_string(),
    ];
    if bool_field(opts.item, "ignoreCase", true) {
        args.push("--ignore-case".to_string());
    }
    if opts.allow_literal && bool_field(opts.item, "literalSearch", false) {
        args.push("--fixed-strings".to_string());
    }
    if opts.include_hidden {
        args.push("--hidden".to_string());
    }
    if context > 0 {
        args.push("--context".to_string());
        args.push(context.to_string());
    }
    for pattern in split_patterns(opts.file_pattern) {
        args.push("--glob".to_string());
        args.push(pattern);
    }
    args.push("--".to_string());
    args.push(opts.pattern.to_string());
    args.push(opts.path.display().to_string());

    let output = run_bundled(BundledTool::Rg, &args, None, timeout_ms)?;
    if !matches!(output.status_code, Some(0) | Some(1)) {
        return Err(format!(
            "bundled rg failed with code {:?}: {}",
            output.status_code,
            output.stderr.trim()
        ));
    }

    Ok(SearchSession {
        lines: parse_rg_json(&output.stdout, opts.max_results)?,
        backend: output.backend.to_string(),
    })
}

fn parse_rg_json(stdout: &str, max_results: usize) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    for line in stdout.lines() {
        if lines.len() >= max_results {
            break;
        }

        let value = serde_json::from_str::<Value>(line)
            .map_err(|error| format!("Failed to parse bundled rg JSON: {error}"))?;
        let Some(event_type) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        if event_type != "match" && event_type != "context" {
            continue;
        }

        let Some(data) = value.get("data") else {
            continue;
        };
        let Some(path) = data
            .get("path")
            .and_then(|path| path.get("text"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let line_number = data.get("line_number").and_then(Value::as_u64).unwrap_or(0);
        let text = data
            .get("lines")
            .and_then(|lines| lines.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_end_matches(['\r', '\n']);
        let sep = if event_type == "match" { ":" } else { "-" };
        lines.push(format!("{path}{sep}{line_number}:{text}"));
    }

    Ok(lines)
}

fn file_search_match(
    root: &Path,
    path_text: &str,
    pattern: &str,
    file_patterns: &[String],
    ignore_case: bool,
) -> bool {
    let path = Path::new(path_text);
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let rel = path
        .strip_prefix(root)
        .ok()
        .and_then(|value| value.to_str())
        .unwrap_or(path_text)
        .replace('\\', "/");
    if !file_patterns.is_empty()
        && !file_patterns
            .iter()
            .any(|item| glob_match(item, name, ignore_case) || glob_match(item, &rel, ignore_case))
    {
        return false;
    }
    if is_exact_filename(pattern) {
        return text_eq(name, pattern, ignore_case);
    }
    if is_glob_pattern(pattern) {
        return glob_match(pattern, name, ignore_case) || glob_match(pattern, &rel, ignore_case);
    }
    text_contains(name, pattern, ignore_case) || text_contains(&rel, pattern, ignore_case)
}

fn is_exact_filename(pattern: &str) -> bool {
    pattern.rsplit_once('.').is_some() && !is_glob_pattern(pattern)
}

fn is_glob_pattern(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|item| matches!(item, '*' | '?' | '[' | '{' | ']' | '}'))
}

fn glob_match(pattern: &str, value: &str, ignore_case: bool) -> bool {
    let mut regex = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            _ => regex.push_str(&regex::escape(&ch.to_string())),
        }
    }
    regex.push('$');
    RegexBuilder::new(&regex)
        .case_insensitive(ignore_case)
        .build()
        .map(|regex| regex.is_match(value))
        .unwrap_or(false)
}

fn text_eq(left: &str, right: &str, ignore_case: bool) -> bool {
    if ignore_case {
        return left.eq_ignore_ascii_case(right);
    }
    left == right
}

fn text_contains(value: &str, pattern: &str, ignore_case: bool) -> bool {
    if ignore_case {
        return value.to_lowercase().contains(&pattern.to_lowercase());
    }
    value.contains(pattern)
}

fn split_patterns(pattern: Option<&str>) -> Vec<String> {
    pattern
        .into_iter()
        .flat_map(|value| value.split('|'))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn read_pattern(item: &Value) -> Result<String, String> {
    if let Some(path) = item.get("pattern_path").and_then(Value::as_str) {
        let path = ensure_path_allowed(path)?;
        let offset = item
            .get("pattern_offset")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let length = item
            .get("pattern_length")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        return read_text_slice(path, offset, length);
    }

    item.get("pattern")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "pattern or pattern_path is required".to_string())
}

fn sessions() -> &'static Mutex<HashMap<String, SearchSession>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn bool_field(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_pattern_is_error() {
        let result = handle_search_regex(&json!({ "items": [{ "path": "." }] }));
        assert!(result.is_error);
    }
}
