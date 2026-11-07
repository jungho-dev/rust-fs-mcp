//! mod.rs
//! tools::search::mod
//!
//! Content regex search tool: fs-search.
//! In-process engine built on ripgrep's own libraries (grep-searcher + ignore parallel
//! walk): removes the ~16ms per-search rg spawn and the PATH dependency while keeping
//! the rg-compatible contract (default excludes, filePattern globs, context lines,
//! maxResults early stop, timeout, literal fallback, "path / N:line" output shape).
//! Submodules: matcher (regex engines, smart-case), walk (parallel walk, sinks, budgets).
//!

mod matcher;
#[cfg(test)]
mod tests;
mod walk;

use crate::core::batch::{SEARCH_PLAN, create_batch_response, run_batch_parallel};
use crate::core::config::ensure_path_allowed;
use crate::core::response::{RawResult, max_output_bytes};
use crate::tools::common::{bool_field, item_content, path_in_heavy_dir};
use grep_searcher::Encoding;
use matcher::{
    PatternOpts, SearchMatcher, Unsupported, build_fancy_matcher, build_matcher,
    classify_unsupported, has_uppercase_literal, parse_error_gist,
};
use serde_json::{Value, json};
use walk::{SearchSpec, run_native_search};

const SEARCH_BACKEND: &str = "native-grep";
// 기본 타임아웃은 Codex 계열 tools/call 30초 상한 안쪽이면서 대형 트리를 감당하는 25초.
const SEARCH_TIMEOUT_MS: u64 = 25_000;
// 줄 길이 상한 (rg --max-columns)
const DEFAULT_MAX_COLUMNS: usize = 500;

#[derive(Clone, Debug)]
struct SearchSession {
    lines: Vec<String>,
    backend: String,
    partial: bool,
    timed_out: bool,
    limit_reached: bool,
}

// 1. Search tool --------------------------------------------------------------
pub fn handle_fs_search(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_parallel(items, SEARCH_PLAN, regex_item);
    create_batch_response("fs-search", results, true)
}

fn regex_item(item: &Value) -> RawResult {
    let search = match run_regex_search(item) {
        Ok(search) => search,
        Err(error) => return RawResult::error(error),
    };
    // The match lines ship in the text body once; structured carries metadata only instead of
    // re-sending the same lines as a JSON array.
    // 상태 표식 본문 앞 배치 (절단 대비)
    let mut notes = Vec::new();
    if search.timed_out {
        notes.push("[search stopped at the time budget; results are partial. Narrow the path or raise timeout_ms]");
    }
    if search.limit_reached {
        notes.push("[stopped at maxResults or the output budget; more matches may exist. Narrow with filePattern or a deeper path]");
    }
    notes.extend(search.lines.iter().map(String::as_str));
    let text = notes.join("\n");
    let backend = search.backend;
    let total = search.lines.len();
    let mut structured = json!({
        "backend": &backend,
        "totalCount": total
    });
    for (key, flag) in [
        ("partial", search.partial),
        ("timedOut", search.timed_out),
        ("limitReached", search.limit_reached),
    ] {
        if flag {
            structured[key] = json!(true);
        }
    }
    let mut result = RawResult::structured(text, structured);
    result.meta.insert("backend".to_string(), json!(backend));
    result
}

// 2. In-process search runner -------------------------------------------------
fn run_regex_search(item: &Value) -> Result<SearchSession, String> {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return Err("path must be a string".to_string());
    };
    let root = ensure_path_allowed(path)?;
    // 없는 경로 오류 처리
    if std::fs::metadata(&root).is_err() {
        return Err(format!("Path does not exist: {}", root.display()));
    }
    let pattern = read_pattern(item)?;
    let opts = PatternOpts {
        // 미지정 시 smart-case
        ignore_case: item
            .get("ignoreCase")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                !has_uppercase_literal(&pattern, bool_field(item, "literal", false))
            }),
        literal: bool_field(item, "literal", false),
        multiline: bool_field(item, "multiline", false),
        word_match: bool_field(item, "wordMatch", false),
    };
    let include_hidden = bool_field(item, "includeHidden", false);
    let max_results = item
        .get("maxResults")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(usize::MAX);
    if max_results == 0 {
        return Ok(SearchSession {
            lines: Vec::new(),
            backend: SEARCH_BACKEND.to_string(),
            partial: false,
            timed_out: false,
            limit_reached: false,
        });
    }
    let encoding = match item.get("encoding").and_then(Value::as_str) {
        Some(label) => Some(
            Encoding::new(label).map_err(|error| format!("Invalid encoding '{label}': {error}"))?,
        ),
        None => None,
    };
    let max_columns = match item.get("maxColumns").and_then(Value::as_u64) {
        Some(0) => usize::MAX,
        Some(value) => value as usize,
        None => DEFAULT_MAX_COLUMNS,
    };
    let spec = SearchSpec {
        include_hidden,
        respect_ignore: bool_field(item, "respectGitignore", true),
        file_globs: split_patterns(item.get("filePattern").and_then(Value::as_str)),
        // 기본 제외: 대형 산출물 디렉터리(node_modules/target, hidden 시 .git)가 시간 예산
        // 소진의 주범. 검색 루트가 그 내부이거나 noDefaultExcludes:true면 적용하지 않는다.
        default_excludes: !bool_field(item, "noDefaultExcludes", false)
            && !path_in_heavy_dir(&root),
        context: item
            .get("contextLines")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(2),
        max_results,
        max_columns,
        // 응답 예산 80% 수집 (이스케이프 여유)
        byte_budget: max_output_bytes() * 4 / 5,
        timeout_ms: item
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(SEARCH_TIMEOUT_MS),
        multiline: opts.multiline,
        encoding,
        root,
    };
    let (matcher, backend) = if opts.literal {
        // 명시 literal은 rg -F 계약: 메타문자 이스케이프 없이 고정 문자열을 찾는다.
        let matcher = build_matcher(&pattern, &opts, true)
            .map_err(|error| format!("Invalid literal pattern: {error}"))?;
        (
            SearchMatcher::Linear(matcher),
            format!("{SEARCH_BACKEND} (literal)"),
        )
    } else {
        match build_matcher(&pattern, &opts, false) {
            Ok(matcher) => (SearchMatcher::Linear(matcher), SEARCH_BACKEND.to_string()),
            Err(error) => match classify_unsupported(&error) {
                // 크기 초과는 fancy로도 해결 불가: 반복 축소/분할 힌트로 거절한다.
                Some(Unsupported::TooBig(limit)) => {
                    return Err(format!(
                        "pattern compiles past the {limit}-byte engine limit; shrink bounded repetitions or split the search"
                    ));
                }
                // lookaround/backreference는 backtracking 엔진(fancy-regex)으로 실제 매칭한다.
                Some(Unsupported::Fancy) => {
                    let matcher = build_fancy_matcher(&pattern, &opts).map_err(|inner| {
                        format!("Invalid search pattern (backtracking engine): {inner}")
                    })?;
                    (
                        SearchMatcher::Fancy(matcher),
                        format!("{SEARCH_BACKEND} (fancy: lookaround/backreference)"),
                    )
                }
                // 순수 문법 오류만 리터럴 검색으로 1회 폴백하고 라벨에 파스 오류 요지를 남긴다.
                None => {
                    let matcher = build_matcher(&pattern, &opts, true)
                        .map_err(|inner| format!("Invalid search pattern: {inner}"))?;
                    (
                        SearchMatcher::Linear(matcher),
                        format!(
                            "{SEARCH_BACKEND} (literal fallback: {})",
                            parse_error_gist(&error)
                        ),
                    )
                }
            },
        }
    };
    let outcome = run_native_search(&spec, &matcher)?;
    // 수집분 있는 타임아웃은 부분 성공
    if outcome.timed_out && outcome.lines.is_empty() {
        return Err(format!(
            "{SEARCH_BACKEND} timed out after {}ms with no matches yet; narrow the search path, add filePattern, or raise timeout_ms",
            spec.timeout_ms
        ));
    }
    // 읽지 못한 파일(잠김 등)이 있어도 수집된 매치는 부분 결과로 살린다.
    let backend = if outcome.partial {
        format!("{backend} (partial: some files were unreadable)")
    } else {
        backend
    };
    Ok(SearchSession {
        lines: outcome.lines,
        backend,
        partial: outcome.partial,
        timed_out: outcome.timed_out,
        limit_reached: outcome.limit_reached,
    })
}

// 3. Argument readers ---------------------------------------------------------
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
    // 다른 도구 인자를 들고 온 혼동 호출은 교정 힌트로 안내.
    if item.get("pattern_path").and_then(Value::as_str).is_none()
        && (item.get("start_line").is_some() || item.get("line_count").is_some())
    {
        return Err(
            "pattern is required; fs-search matches regex content — for start_line/line_count reads use file-read-line-range"
                .to_string(),
        );
    }
    item_content(item, "pattern", "pattern_path")
}
