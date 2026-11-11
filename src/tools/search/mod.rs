//! mod.rs
//! tools::search::mod
//!
//! Content regex search tool: fs-search.
//! In-process engine built on ripgrep's own libraries (grep-searcher + ignore parallel
//! walk): removes the ~16ms per-search rg spawn and the PATH dependency while keeping
//! the rg-compatible contract (default excludes, filePattern globs, context lines,
//! maxResults early stop, timeout, literal fallback, "path / N:line" output shape).
//! Submodules: matcher (regex engines, smart-case), walk (parallel walk, sinks, budgets),
//! find (path-find, fd-style name and metadata filters), ast (ast-search over the ast-grep CLI).
//! A fancy-regex item runs whole in a kill-able worker process (core::isolate), killed at timeout_ms + 1 s.
//!

mod ast;
pub(crate) mod find;
mod matcher;
#[cfg(test)]
mod tests;
mod walk;

pub use ast::handle_ast_search;
pub(crate) use ast::{AstEdit, AstRewriteSpec, collect_ast_edits};
pub use find::handle_path_find;

use crate::core::batch::{SEARCH_PLAN, create_batch_response, run_batch_parallel};
use crate::core::config::ensure_path_allowed;
use crate::core::isolate::{WorkerError, run_in_worker};
use crate::core::response::{RawResult, max_output_bytes};
use crate::tools::args::common::{bool_field, item_content, path_in_heavy_dir};
use grep_searcher::Encoding;
use matcher::{
    PatternOpts, SearchMatcher, Unsupported, build_fancy_matcher, build_matcher,
    classify_unsupported, has_uppercase_literal, is_fancy_syntax_error, parse_error_gist,
};
use serde_json::{Value, json};
use std::time::Duration;
use walk::{SearchSpec, run_native_search};

const SEARCH_BACKEND: &str = "native-grep";
// 기본 타임아웃은 Codex 계열 tools/call 30초 상한 안쪽이면서 대형 트리를 감당하는 25초.
const SEARCH_TIMEOUT_MS: u64 = 25_000;
// 줄 길이 상한 (rg --max-columns)
const DEFAULT_MAX_COLUMNS: usize = 500;
// 워커 kill 유예 (자체 timeout_ms 반환 여유)
const WORKER_GRACE_MS: u64 = 1_000;

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
    search_item(item, max_output_bytes() * 4 / 5, true)
}
// fancy 항목만 격리 워커 (나머지 무오버헤드)
fn search_item(item: &Value, byte_budget: usize, isolate_fancy: bool) -> RawResult {
    let Prepared {
        spec,
        matcher,
        backend,
    } = match prepare_search(item, byte_budget) {
        Ok(Some(prepared)) => prepared,
        // maxResults 0
        Ok(None) => {
            return render_session(SearchSession {
                lines: Vec::new(),
                backend: SEARCH_BACKEND.to_string(),
                partial: false,
                timed_out: false,
                limit_reached: false,
            });
        }
        Err(error) => return RawResult::error(error),
    };
    if isolate_fancy && matches!(matcher, SearchMatcher::Fancy(_)) {
        match isolated_search(item, byte_budget, spec.timeout_ms) {
            Err(WorkerError::Unavailable) => {}
            Ok(result) => return result,
            Err(error) => return RawResult::error(describe_worker_error(&error, spec.timeout_ms)),
        }
    }
    match finish_search(&spec, &matcher, backend) {
        Ok(search) => render_session(search),
        Err(error) => RawResult::error(error),
    }
}
fn render_session(search: SearchSession) -> RawResult {
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
struct Prepared {
    spec: SearchSpec,
    matcher: SearchMatcher,
    backend: String,
}
fn prepare_search(item: &Value, byte_budget: usize) -> Result<Option<Prepared>, String> {
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
        return Ok(None);
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
        byte_budget,
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
                Some(Unsupported::Fancy) => match build_fancy_matcher(&pattern, &opts) {
                    Ok(matcher) => (
                        SearchMatcher::Fancy(matcher),
                        format!("{SEARCH_BACKEND} (fancy: lookaround/backreference)"),
                    ),
                    // fancy 문법 오류도 리터럴 1회 폴백
                    Err(inner) if is_fancy_syntax_error(&inner) => {
                        let matcher = build_matcher(&pattern, &opts, true)
                            .map_err(|escaped| format!("Invalid search pattern: {escaped}"))?;
                        (
                            SearchMatcher::Linear(matcher),
                            format!("{SEARCH_BACKEND} (literal fallback: {inner})"),
                        )
                    }
                    Err(inner) => {
                        return Err(format!(
                            "Invalid search pattern (backtracking engine): {inner}"
                        ));
                    }
                },
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
    Ok(Some(Prepared {
        spec,
        matcher,
        backend,
    }))
}
fn finish_search(
    spec: &SearchSpec,
    matcher: &SearchMatcher,
    backend: String,
) -> Result<SearchSession, String> {
    let outcome = run_native_search(spec, matcher)?;
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

// 3. Isolated fancy-regex search ----------------------------------------------
// 부모: 항목 전체를 워커에 위임, 기한 초과 시 kill
fn isolated_search(
    item: &Value,
    byte_budget: usize,
    timeout_ms: u64,
) -> Result<RawResult, WorkerError> {
    let job = json!({ "op": "fs-search", "item": item, "byteBudget": byte_budget });
    let kill_after = Duration::from_millis(timeout_ms.saturating_add(WORKER_GRACE_MS));
    let mut reply = run_in_worker(&job, kill_after)?;
    let Some(Value::Array(content)) = reply.get_mut("content").map(Value::take) else {
        return Err(WorkerError::Protocol(
            "fs-search reply has no content array".to_string(),
        ));
    };
    let Some(is_error) = reply["isError"].as_bool() else {
        return Err(WorkerError::Protocol(
            "fs-search reply has no isError flag".to_string(),
        ));
    };
    let mut structured = reply.get_mut("structured").map(Value::take);
    if let Some(Value::Object(fields)) = &mut structured {
        fields.insert("isolated".to_string(), json!(true));
    }
    let meta = match reply.get_mut("meta").map(Value::take) {
        Some(Value::Object(meta)) => meta,
        _ => serde_json::Map::new(),
    };
    Ok(RawResult {
        content,
        structured: structured.filter(|value| !value.is_null()),
        is_error,
        meta,
    })
}
fn describe_worker_error(error: &WorkerError, timeout_ms: u64) -> String {
    match error {
        WorkerError::TimedOut(killed_after) => format!(
            "fancy-regex search stopped: the backtracking engine could not stop at timeout_ms {timeout_ms} ms, so its isolated worker was killed after {} ms and no partial results exist; simplify the look-around or narrow the path",
            killed_after.as_millis()
        ),
        other => format!("fancy-regex search failed: {other}"),
    }
}
// 워커: 같은 항목 로직을 프로세스 내 실행
pub fn run_isolated_search(job: &Value) -> Result<Value, String> {
    let Some(byte_budget) = job["byteBudget"]
        .as_u64()
        .and_then(|budget| usize::try_from(budget).ok())
    else {
        return Err("fs-search job needs a byteBudget".to_string());
    };
    let result = search_item(&job["item"], byte_budget, false);
    Ok(json!({
        "content": result.content,
        "structured": result.structured,
        "isError": result.is_error,
        "meta": result.meta
    }))
}

// 4. Argument readers ---------------------------------------------------------
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
#[cfg(test)]
mod worker_tests {
    use super::*;

    fn fixture_dir(prefix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "foobar foobaz\nfoobar\n").unwrap();
        dir
    }

    #[test]
    fn huge_timeout_ms_does_not_overflow_the_deadline() {
        let dir = fixture_dir("rust-fs-mcp-search-huge-timeout");
        for pattern in ["foo", "foo(?=bar)"] {
            let result = handle_fs_search(&json!({ "items": [{
                "path": dir.display().to_string(),
                "pattern": pattern,
                "timeout_ms": u64::MAX
            }] }));
            assert!(!result.is_error, "{pattern}: {result:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn worker_job_runs_the_same_item_in_process() {
        let dir = fixture_dir("rust-fs-mcp-search-worker-job");
        let item = json!({ "path": dir.display().to_string(), "pattern": "foo(?=bar)" });
        let local = regex_item(&item);
        assert!(!local.is_error, "{local:?}");
        assert!(local.structured.as_ref().unwrap().get("isolated").is_none());
        let reply = run_isolated_search(&json!({
            "item": item,
            "byteBudget": max_output_bytes() * 4 / 5
        }))
        .unwrap();
        assert_eq!(reply["content"], json!(local.content));
        assert_eq!(reply["structured"], local.structured.clone().unwrap());
        assert_eq!(reply["isError"], false);
        let missing = run_isolated_search(&json!({ "item": {} }));
        assert!(missing.unwrap_err().contains("byteBudget"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn killed_worker_error_names_the_fancy_search() {
        let killed =
            describe_worker_error(&WorkerError::TimedOut(Duration::from_millis(2500)), 1500);
        assert!(
            killed.starts_with("fancy-regex search stopped")
                && killed.contains("timeout_ms 1500 ms")
                && killed.contains("killed after 2500 ms"),
            "{killed}"
        );
        let crashed = describe_worker_error(
            &WorkerError::Crashed {
                code: Some(101),
                stderr: "panicked at fancy".to_string(),
            },
            1500,
        );
        assert!(
            crashed.contains("exit code 101: panicked at fancy"),
            "{crashed}"
        );
    }
    #[test]
    fn fancy_syntax_errors_fall_back_to_literal_at_user_positions() {
        let dir = fixture_dir("rust-fs-mcp-search-fancy-fallback");
        std::fs::write(
            dir.join("b.txt"),
            "lit (?<=x)a)|(b end\nonly a here\nonly b here\nlit (a)\\1)|(b end\n",
        )
        .unwrap();
        let path = dir.display().to_string();
        for (pattern, line, position) in [(r"(?<=x)a)|(b", 1, 7), (r"(a)\1)|(b", 4, 5)] {
            for word_match in [false, true] {
                let response = crate::tools::dispatch_tool_call(
                    "fs-search",
                    Some(json!({ "items": [{
                        "path": path,
                        "pattern": pattern,
                        "wordMatch": word_match,
                        "filePattern": "b.txt",
                        "contextLines": 0
                    }] })),
                );
                let status = response["_meta"]["fsMcpResult"]["status"].as_str();
                assert_eq!(status, Some("success"), "{pattern}: {response}");
                let text = response["structuredContent"]["data"]["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default();
                // wordMatch 리터럴은 \b 경계 ("(" 앞 경계 없음)
                assert_eq!(
                    text.contains(&format!("{line}:lit {pattern} end")),
                    !word_match,
                    "{text}"
                );
                assert!(!text.contains("only"), "{text}");
                let backend = response["structuredContent"]["data"]["structuredContent"]["results"]
                    [0]["data"]["backend"]
                    .as_str()
                    .unwrap_or_default();
                assert!(backend.contains("literal fallback"), "{backend}");
                assert!(
                    backend.contains(&format!("Parsing error at position {position}:")),
                    "{pattern}: {backend}"
                );
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn unsupported_fancy_constructs_stay_errors() {
        let dir = fixture_dir("rust-fs-mcp-search-fancy-unsupported");
        let item = json!({ "path": dir.display().to_string(), "pattern": r"(?<=a+)b" });
        let result = regex_item(&item);
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(result.is_error, "{result:?}");
        assert!(text.contains("backtracking engine"), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
