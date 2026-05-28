//! search_tools.rs
//! tools::search_tools
//!
//! Session-based search tools: search-start / search-regex / search-get / search-stop.
//! Calls ripgrep and fd from PATH as external commands and paginates results from an in-memory session.
//!

use crate::core::args_ref::read_text_slice;
use crate::core::batch::{create_batch_response, run_batch, run_batch_parallel};
use crate::core::config::ensure_path_allowed;
use crate::core::external::{ExternalTool, run_external};
use crate::core::response::RawResult;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

#[derive(Clone, Debug)]
struct SearchSession {
    lines: Vec<String>,
    backend: String,
}

#[derive(Deserialize)]
struct RgEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    data: Option<RgData>,
}

#[derive(Deserialize)]
struct RgData {
    path: Option<RgText>,
    line_number: Option<u64>,
    lines: Option<RgText>,
}

#[derive(Deserialize)]
struct RgText {
    text: String,
}

static SESSIONS: OnceLock<RwLock<HashMap<String, SearchSession>>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static GLOB_CACHE: OnceLock<RwLock<HashMap<(String, bool), Regex>>> = OnceLock::new();

// To avoid amplified RAM leaks from forgotten `search-stop` calls in multi-instance setups,
// per-session line counts and the concurrent session count are capped; the oldest session is evicted on overflow.
const MAX_SEARCH_LINES: usize = 100_000;
const MAX_SEARCH_SESSIONS: usize = 64;

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
    let mut search = match run_start_search(&item) {
        Ok(search) => search,
        Err(error) => return RawResult::error(error),
    };
    let raw_total = search.lines.len();
    let truncated = raw_total > MAX_SEARCH_LINES;
    if truncated {
        search.lines.truncate(MAX_SEARCH_LINES);
    }
    let session_id = format!("search-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let preview = search.lines.iter().take(20).cloned().collect::<Vec<_>>();
    let total = search.lines.len();
    let backend = search.backend.clone();
    {
        let mut map = sessions().write().unwrap();
        evict_oldest_sessions(&mut map);
        map.insert(session_id.clone(), search);
    }

    RawResult::structured(
        format!(
            "{session_id}: {total} results{}",
            if truncated {
                format!(" (truncated from {raw_total}, cap {MAX_SEARCH_LINES})")
            } else {
                String::new()
            }
        ),
        json!({
            "sessionId": session_id,
            "backend": backend,
            "totalCount": total,
            "rawTotalCount": raw_total,
            "truncated": truncated,
            "preview": preview
        }),
    )
}

// When the session map hits the cap, evict the oldest (lowest numeric suffix) session.
// Multi-eviction cases are handled by a single sort, eliminating O(N^2) rescans.
fn evict_oldest_sessions(map: &mut HashMap<String, SearchSession>) {
    if map.len() < MAX_SEARCH_SESSIONS {
        return;
    }
    let mut keys: Vec<(u64, String)> = map
        .keys()
        .map(|id| {
            let order = id
                .strip_prefix("search-")
                .and_then(|suffix| suffix.parse::<u64>().ok())
                .unwrap_or(u64::MAX);
            (order, id.clone())
        })
        .collect();
    keys.sort_by_key(|(order, _)| *order);
    let excess = map.len() + 1 - MAX_SEARCH_SESSIONS;
    for (_, key) in keys.into_iter().take(excess) {
        map.remove(&key);
    }
}

fn regex_item(item: Value) -> RawResult {
    let search = match run_regex_search(&item) {
        Ok(search) => search,
        Err(error) => return RawResult::error(error),
    };
    let text = search.lines.join("\n");
    let backend = search.backend;
    let total = search.lines.len();
    let mut result = RawResult::structured(
        text,
        json!({
            "backend": &backend,
            "totalCount": total,
            "results": search.lines
        }),
    );
    result.meta.insert("backend".to_string(), json!(backend));
    result
}

fn get_item(item: Value) -> RawResult {
    let Some(session_id) = item.get("sessionId").and_then(Value::as_str) else {
        return RawResult::error("sessionId must be a string");
    };

    let sessions = sessions().read().unwrap();
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

    let removed = sessions().write().unwrap().remove(session_id).is_some();
    RawResult::structured(
        format!("Stopped {session_id}: {removed}"),
        json!({ "sessionId": session_id, "removed": removed }),
    )
}

// 2. External search runner ----------------------------------------------------
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
        let backend = if search_type == "files" {
            ExternalTool::Fd.backend_name()
        } else {
            ExternalTool::Rg.backend_name()
        };
        return Ok(SearchSession {
            lines: Vec::new(),
            backend: backend.to_string(),
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
            backend: ExternalTool::Rg.backend_name().to_string(),
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

    let output = run_external(ExternalTool::Fd, &args, None, None)?;
    if output.status_code != Some(0) {
        return Err(format!(
            "external fd failed with code {:?}: {}",
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
        backend: ExternalTool::Fd.backend_name().to_string(),
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

    let output = run_external(ExternalTool::Rg, &args, None, timeout_ms)?;
    if !matches!(output.status_code, Some(0) | Some(1)) {
        return Err(format!(
            "external rg failed with code {:?}: {}",
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

        let event = serde_json::from_str::<RgEvent>(line)
            .map_err(|error| format!("Failed to parse external rg JSON: {error}"))?;
        let Some(event_type) = event.event_type.as_deref() else {
            continue;
        };
        if event_type != "match" && event_type != "context" {
            continue;
        }

        let Some(data) = event.data else {
            continue;
        };
        let Some(path) = data.path.as_ref().map(|path| path.text.as_str()) else {
            continue;
        };
        let line_number = data.line_number.unwrap_or(0);
        let text = data
            .lines
            .as_ref()
            .map(|lines| lines.text.as_str())
            .unwrap_or_default()
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
    // For plain paths without `\` (Unix or already-normalized Windows paths) avoid an extra String allocation.
    let rel_raw = path
        .strip_prefix(root)
        .ok()
        .and_then(|value| value.to_str())
        .unwrap_or(path_text);
    let rel: Cow<'_, str> = if rel_raw.contains('\\') {
        Cow::Owned(rel_raw.replace('\\', "/"))
    } else {
        Cow::Borrowed(rel_raw)
    };
    let rel_str: &str = &rel;
    if !file_patterns.is_empty()
        && !file_patterns.iter().any(|item| {
            glob_match(item, name, ignore_case) || glob_match(item, rel_str, ignore_case)
        })
    {
        return false;
    }
    if is_exact_filename(pattern) {
        return text_eq(name, pattern, ignore_case);
    }
    if is_glob_pattern(pattern) {
        return glob_match(pattern, name, ignore_case) || glob_match(pattern, rel_str, ignore_case);
    }
    text_contains(name, pattern, ignore_case) || text_contains(rel_str, pattern, ignore_case)
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
    let cache = GLOB_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    let key = (pattern.to_string(), ignore_case);
    // Regex is Sync so is_match runs while holding only the read lock, removing the per-call `.cloned()` cost.
    if let Some(regex) = cache.read().unwrap().get(&key) {
        return regex.is_match(value);
    }
    let mut source = String::with_capacity(pattern.len() * 2 + 2);
    source.push('^');
    for ch in pattern.chars() {
        match ch {
            '*' => source.push_str(".*"),
            '?' => source.push('.'),
            // Escape only regex meta-characters; push others as-is (drops the per-char alloc from `regex::escape(&ch.to_string())`).
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
    let Ok(regex) = RegexBuilder::new(&source)
        .case_insensitive(ignore_case)
        .build()
    else {
        return false;
    };
    let mut cache_w = cache.write().unwrap();
    cache_w.entry(key).or_insert(regex).is_match(value)
}

fn text_eq(left: &str, right: &str, ignore_case: bool) -> bool {
    if ignore_case {
        return left.eq_ignore_ascii_case(right);
    }
    left == right
}

fn text_contains(value: &str, pattern: &str, ignore_case: bool) -> bool {
    if !ignore_case {
        return value.contains(pattern);
    }
    if value.is_ascii() && pattern.is_ascii() {
        return ascii_contains_ignore_case(value.as_bytes(), pattern.as_bytes());
    }
    let pattern = pattern.to_lowercase();
    value.to_lowercase().contains(&pattern)
}

fn ascii_contains_ignore_case(value: &[u8], pattern: &[u8]) -> bool {
    if pattern.is_empty() {
        return true;
    }
    if pattern.len() > value.len() {
        return false;
    }
    value.windows(pattern.len()).any(|window| {
        window
            .iter()
            .zip(pattern)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
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

fn sessions() -> &'static RwLock<HashMap<String, SearchSession>> {
    SESSIONS.get_or_init(|| RwLock::new(HashMap::new()))
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

    #[test]
    fn text_contains_uses_ascii_ignore_case() {
        assert!(text_contains("Alpha/BETA.txt", "beta", true));
        assert!(!text_contains("Alpha/BETA.txt", "beta", false));
        assert!(text_contains("한글Alpha", "alpha", true));
    }

    #[test]
    fn evict_oldest_sessions_drops_lowest_id() {
        let mut map: HashMap<String, SearchSession> = HashMap::new();
        for n in 1..=MAX_SEARCH_SESSIONS {
            map.insert(
                format!("search-{n}"),
                SearchSession {
                    lines: Vec::new(),
                    backend: "test".to_string(),
                },
            );
        }
        assert_eq!(map.len(), MAX_SEARCH_SESSIONS);
        evict_oldest_sessions(&mut map);
        assert_eq!(map.len(), MAX_SEARCH_SESSIONS - 1);
        assert!(!map.contains_key("search-1"));
        assert!(map.contains_key(&format!("search-{MAX_SEARCH_SESSIONS}")));
    }
}
