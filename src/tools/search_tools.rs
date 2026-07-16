//! search_tools.rs
//! tools::search_tools
//!
//! Content regex search tool: fs-search.
//! In-process engine built on ripgrep's own libraries (grep-searcher + ignore parallel
//! walk): removes the ~16ms per-search rg spawn and the PATH dependency while keeping
//! the rg-compatible contract (default excludes, filePattern globs, context lines,
//! maxResults early stop, timeout, literal fallback, "path / N:line" output shape).
//!

use crate::core::args_ref::read_text_slice;
use crate::core::batch::{SEARCH_PLAN, available_parallelism, create_batch_response, run_batch_parallel};
use crate::core::config::ensure_path_allowed;
use crate::core::response::RawResult;
use grep_matcher::{Match, Matcher, NoCaptures, NoError};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkState;
use ignore::overrides::OverrideBuilder;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const SEARCH_BACKEND: &str = "native-grep";
// 기본 타임아웃은 Codex 계열 tools/call 30초 상한 안쪽이면서 대형 트리를 감당하는 25초.
const SEARCH_TIMEOUT_MS: u64 = 25_000;

#[derive(Clone, Debug)]
struct SearchSession {
    lines: Vec<String>,
    backend: String,
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
    let text = search.lines.join("\n");
    let backend = search.backend;
    let total = search.lines.len();
    let mut result = RawResult::structured(
        text,
        json!({
            "backend": &backend,
            "totalCount": total
        }),
    );
    result.meta.insert("backend".to_string(), json!(backend));
    result
}

// 2. In-process search runner ----------------------------------------------------
struct SearchSpec {
    root: PathBuf,
    include_hidden: bool,
    file_globs: Vec<String>,
    default_excludes: bool,
    context: usize,
    max_results: usize,
    timeout_ms: u64,
}

fn run_regex_search(item: &Value) -> Result<SearchSession, String> {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return Err("path must be a string".to_string());
    };
    let root = ensure_path_allowed(path)?;
    let pattern = read_pattern(item)?;
    let ignore_case = bool_field(item, "ignoreCase", true);
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
        });
    }
    let spec = SearchSpec {
        include_hidden,
        file_globs: split_patterns(item.get("filePattern").and_then(Value::as_str)),
        // 기본 제외: 대형 산출물 디렉터리(node_modules/target, hidden 시 .git)가 시간 예산
        // 소진의 주범. 검색 루트가 그 내부이거나 noDefaultExcludes:true면 적용하지 않는다.
        default_excludes: !bool_field(item, "noDefaultExcludes", false) && !path_in_heavy_dir(&root),
        context: item
            .get("contextLines")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(2),
        max_results,
        timeout_ms: item
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(SEARCH_TIMEOUT_MS),
        root,
    };
    // 정규식 파스 실패는 리터럴 검색으로 1회 폴백해 오류 대신 결과를 돌려준다.
    let (matcher, backend) = match build_matcher(&pattern, ignore_case, false) {
        Ok(matcher) => (matcher, SEARCH_BACKEND.to_string()),
        Err(_) => {
            let matcher = build_matcher(&pattern, ignore_case, true)
                .map_err(|error| format!("Invalid search pattern: {error}"))?;
            (
                matcher,
                format!("{SEARCH_BACKEND} (literal fallback: regex parse error)"),
            )
        }
    };
    let outcome = run_native_search(&spec, &matcher)?;
    // maxResults 조기 종료는 정상 부분 성공(go의 cancel-and-return과 동일); 순수 타임아웃만 오류.
    if outcome.timed_out {
        return Err(format!(
            "{SEARCH_BACKEND} timed out after {}ms; narrow the search path, add filePattern, or raise timeout_ms",
            spec.timeout_ms
        ));
    }
    // 읽지 못한 파일(잠김 등)이 있어도 수집된 매치는 부분 결과로 살린다.
    let backend = if outcome.partial {
        format!("{backend} (partial: some files were unreadable)")
    }
    else {
        backend
    };
    Ok(SearchSession {
        lines: outcome.lines,
        backend,
    })
}

// 2a. regex(bytes) -> grep Matcher adapter -----------------------------------------
// rg와 동일하게 라인 지향 ^/$ 매칭을 위해 multi_line을 켤다. `.`은 개행을 매치하지
// 않으므로(기본) 매치는 실질적으로 한 줄 안에 갇힌다.
#[derive(Clone, Debug)]
struct RegexAdapter {
    regex: regex::bytes::Regex,
}
impl Matcher for RegexAdapter {
    type Captures = NoCaptures;
    type Error = NoError;
    fn find_at(&self, haystack: &[u8], at: usize) -> Result<Option<Match>, NoError> {
        Ok(self
            .regex
            .find_at(haystack, at)
            .map(|found| Match::new(found.start(), found.end())))
    }
    fn new_captures(&self) -> Result<NoCaptures, NoError> {
        Ok(NoCaptures::new())
    }
}
fn build_matcher(pattern: &str, ignore_case: bool, literal: bool) -> Result<RegexAdapter, regex::Error> {
    let source = if literal {
        regex::escape(pattern)
    }
    else {
        pattern.to_string()
    };
    let regex = regex::bytes::RegexBuilder::new(&source)
        .case_insensitive(ignore_case)
        .multi_line(true)
        .build()?;
    Ok(RegexAdapter { regex })
}

// 2b. Parallel walk + per-file sink ---------------------------------------------
struct NativeOutcome {
    lines: Vec<String>,
    timed_out: bool,
    partial: bool,
}
struct Collected {
    lines: Vec<String>,
    hits: usize,
}
fn run_native_search(spec: &SearchSpec, matcher: &RegexAdapter) -> Result<NativeOutcome, String> {
    let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms);
    let collected = Mutex::new(Collected {
        lines: Vec::new(),
        hits: 0,
    });
    let timed_out = AtomicBool::new(false);
    let partial = AtomicBool::new(false);
    let done = AtomicBool::new(false);

    // filePattern / 기본 제외는 rg --glob과 동일한 overrides 시맨틱으로 적용한다.
    let mut overrides = OverrideBuilder::new(&spec.root);
    for glob in &spec.file_globs {
        overrides
            .add(glob)
            .map_err(|error| format!("Invalid filePattern glob '{glob}': {error}"))?;
    }
    if spec.default_excludes {
        for glob in ["!**/node_modules/**", "!**/target/**"] {
            overrides.add(glob).map_err(|error| error.to_string())?;
        }
        if spec.include_hidden {
            overrides.add("!**/.git/**").map_err(|error| error.to_string())?;
        }
    }
    let overrides = overrides.build().map_err(|error| error.to_string())?;

    // rg CLI와 동일 기본: hidden 스킵(includeHidden으로 해제), --no-ignore, 심링크 미추적.
    let mut builder = ignore::WalkBuilder::new(&spec.root);
    builder
        .hidden(!spec.include_hidden)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .follow_links(false)
        .overrides(overrides)
        .threads(available_parallelism().clamp(2, 12));
    builder.build_parallel().run(|| {
        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .before_context(spec.context)
            .after_context(spec.context)
            .binary_detection(BinaryDetection::quit(0))
            .build();
        let matcher = matcher.clone();
        let collected = &collected;
        let timed_out = &timed_out;
        let partial = &partial;
        let done = &done;
        Box::new(move |entry| {
            if done.load(Ordering::Relaxed) || timed_out.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            if Instant::now() >= deadline {
                timed_out.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                // 순회 오류(권한 등)는 해당 항목만 건너뛴다.
                partial.store(true, Ordering::Relaxed);
                return WalkState::Continue;
            };
            if !entry.file_type().map(|kind| kind.is_file()).unwrap_or(false) {
                return WalkState::Continue;
            }
            // 남은 전역 예산 스냅샷: 파일 안에서는 이 한도까지만 수집하고 병합 시 재절단한다.
            let budget = {
                let collected = collected.lock().unwrap();
                if collected.hits >= spec.max_results {
                    done.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
                spec.max_results - collected.hits
            };
            let mut sink = FileSink {
                lines: Vec::new(),
                hits: 0,
                budget,
                deadline,
                timed_out,
                tick: 0,
            };
            if searcher.search_path(&matcher, entry.path(), &mut sink).is_err() {
                // 읽기 실패(잠긴 파일 등)는 부분 결과로 계속한다(rg 종료코드 2 대응).
                partial.store(true, Ordering::Relaxed);
                return WalkState::Continue;
            }
            if timed_out.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            if sink.lines.is_empty() {
                return WalkState::Continue;
            }
            let mut collected = collected.lock().unwrap();
            let remaining = spec.max_results.saturating_sub(collected.hits);
            if remaining == 0 {
                done.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            // Emit the file path once per file run (rg --heading style).
            collected.lines.push(entry.path().display().to_string());
            let take = sink.lines.len().min(remaining);
            collected.lines.extend(sink.lines.drain(..take));
            collected.hits += take;
            if collected.hits >= spec.max_results {
                done.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            WalkState::Continue
        })
    });

    let collected = collected
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let hit_cap = done.into_inner();
    Ok(NativeOutcome {
        lines: collected.lines,
        timed_out: timed_out.into_inner() && !hit_cap,
        partial: partial.into_inner(),
    })
}
// 파일 1개 분량의 매치/컨텍스트를 "N:text" / "N-text"로 수집하는 sink.
struct FileSink<'a> {
    lines: Vec<String>,
    hits: usize,
    budget: usize,
    deadline: Instant,
    timed_out: &'a AtomicBool,
    tick: u32,
}
impl FileSink<'_> {
    fn push_line(&mut self, number: Option<u64>, sep: char, bytes: &[u8]) -> Result<bool, std::io::Error> {
        // 시계 호출은 64회에 1회만: 핫루프에서 Instant::now()가 줄당 비용을 지배하지 않게.
        self.tick = self.tick.wrapping_add(1);
        if self.tick & 63 == 0 && Instant::now() >= self.deadline {
            self.timed_out.store(true, Ordering::Relaxed);
            return Ok(false);
        }
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim_end_matches(['\r', '\n']);
        let number = number.unwrap_or(0);
        self.lines.push(format!("{number}{sep}{text}"));
        self.hits += 1;
        Ok(self.hits < self.budget)
    }
}
impl Sink for FileSink<'_> {
    type Error = std::io::Error;
    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        self.push_line(mat.line_number(), ':', mat.bytes())
    }
    fn context(&mut self, _searcher: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, std::io::Error> {
        self.push_line(ctx.line_number(), '-', ctx.bytes())
    }
}

// 3. Shared helpers -------------------------------------------------------------
// 검색 루트가 기본 제외 대상 디렉터리 내부인지 판별(명시 탐색 의도는 존중). fs-inspect도 공유.
pub(crate) fn path_in_heavy_dir(path: &std::path::Path) -> bool {
    path.components().any(|component| {
        let text = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        text == "node_modules" || text == "target" || text == ".git"
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

    // 다른 도구 인자를 들고 온 혼동 호출은 교정 힌트로 안내.
    if item.get("start_line").is_some() || item.get("line_count").is_some() {
        return Err(
            "pattern is required; fs-search matches regex content — for start_line/line_count reads use file-read-line-range"
                .to_string(),
        );
    }
    item.get("pattern")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "pattern or pattern_path is required".to_string())
}

fn bool_field(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        // target/ 아래를 피해야 default_excludes(=heavy dir 판정) 경로가 유지된다.
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn first_backend(result: &RawResult) -> String {
        result.structured.clone().unwrap()["results"][0]["data"]["backend"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn missing_pattern_is_error() {
        let result = handle_fs_search(&json!({ "items": [{ "path": "." }] }));
        assert!(result.is_error);
    }

    #[test]
    fn line_range_args_get_targeted_hint() {
        let result = handle_fs_search(
            &json!({ "items": [{ "path": ".", "start_line": 3, "line_count": 2 }] }),
        );
        assert!(result.is_error);
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("file-read-line-range"), "{text}");
    }

    #[test]
    fn invalid_regex_falls_back_to_literal() {
        // "sendCancel(" 는 regex 파스 오류 → 리터럴 폴백 매치.
        let dir = temp_dir("rust-fs-mcp-litfb");
        std::fs::write(dir.join("sample.txt"), "call sendCancel( now\n").unwrap();

        let result = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "sendCancel(" }]
        }));
        assert!(!result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("sendCancel("), "{text}");
        assert!(first_backend(&result).contains("literal fallback"), "{result:?}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn groups_lines_under_file_headings_with_context() {
        let dir = temp_dir("rust-fs-mcp-heading");
        std::fs::write(dir.join("a.txt"), "ctx before\nneedle hit\nctx after\n").unwrap();

        let result = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 1 }]
        }));
        assert!(!result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        // rg --heading 형태: 파일 경로 1줄 + "1-ctx", "2:hit", "3-ctx".
        assert!(text.contains("a.txt"), "{text}");
        assert!(text.contains("1-ctx before"), "{text}");
        assert!(text.contains("2:needle hit"), "{text}");
        assert!(text.contains("3-ctx after"), "{text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn default_excludes_skip_heavy_dirs() {
        let dir = temp_dir("rust-fs-mcp-heavy");
        std::fs::create_dir_all(dir.join("node_modules").join("pkg")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("node_modules").join("pkg").join("dep.js"), "needle\n").unwrap();
        std::fs::write(dir.join("target").join("out.txt"), "needle\n").unwrap();
        std::fs::write(dir.join("src").join("app.js"), "needle\n").unwrap();

        let result = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "needle" }]
        }));
        assert!(!result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("app.js"), "{text}");
        assert!(!text.contains("dep.js"), "{text}");
        assert!(!text.contains("out.txt"), "{text}");

        // noDefaultExcludes:true 면 산출물 디렉터리도 검색된다.
        let all = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "needle", "noDefaultExcludes": true }]
        }));
        let text = all.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("dep.js"), "{text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn max_results_caps_match_and_context_lines() {
        let dir = temp_dir("rust-fs-mcp-maxres");
        let body: String = (1..=50).map(|index| format!("needle line {index}\n")).collect();
        std::fs::write(dir.join("many.txt"), body).unwrap();

        let result = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "needle", "maxResults": 5, "contextLines": 0 }]
        }));
        assert!(!result.is_error, "{result:?}");
        let structured = result.structured.clone().unwrap();
        // totalCount = 헤딩 1 + 콘텐츠 줄 5.
        assert_eq!(structured["results"][0]["data"]["totalCount"], 6, "{structured}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn single_file_root_searches_that_file() {
        let dir = temp_dir("rust-fs-mcp-singlefile");
        let file = dir.join("only.rs");
        std::fs::write(&file, "fn needle_here() {}\n").unwrap();

        let result = handle_fs_search(&json!({
            "items": [{ "path": file.display().to_string(), "pattern": "needle_here" }]
        }));
        assert!(!result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("1:fn needle_here() {}"), "{text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_pattern_narrows_targets() {
        let dir = temp_dir("rust-fs-mcp-filepat");
        std::fs::write(dir.join("a.rs"), "needle\n").unwrap();
        std::fs::write(dir.join("b.js"), "needle\n").unwrap();

        let result = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "needle", "filePattern": "*.rs" }]
        }));
        assert!(!result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("b.js"), "{text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
