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
use crate::core::batch::{
    SEARCH_PLAN, available_parallelism, create_batch_response, run_batch_parallel,
};
use crate::core::config::ensure_path_allowed;
use crate::core::encoding::decode_line;
use crate::core::response::{RawResult, max_output_bytes};
use grep_matcher::{Match, Matcher, NoCaptures, NoError};
use grep_searcher::{
    BinaryDetection, Encoding, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch,
};
use ignore::WalkState;
use ignore::overrides::OverrideBuilder;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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

// 2. In-process search runner ----------------------------------------------------
struct SearchSpec {
    root: PathBuf,
    include_hidden: bool,
    respect_ignore: bool,
    file_globs: Vec<String>,
    default_excludes: bool,
    context: usize,
    max_results: usize,
    max_columns: usize,
    byte_budget: usize,
    timeout_ms: u64,
    multiline: bool,
    encoding: Option<Encoding>,
}

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
// smart-case 대문자 판정 (이스케이프·그룹명 제외)
fn has_uppercase_literal(pattern: &str, literal: bool) -> bool {
    if literal {
        return pattern.chars().any(char::is_uppercase);
    }
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                // \p{Lu}, \x{41}, \u{..}: 중괄호 본문 통째로 건너뜀
                Some('p' | 'P' | 'x' | 'u' | 'U') if chars.peek() == Some(&'{') => {
                    for inner in chars.by_ref() {
                        if inner == '}' {
                            break;
                        }
                    }
                }
                // \pL 한 글자 클래스, \x41 두 자리 16진수
                Some('p' | 'P') => {
                    chars.next();
                }
                Some('x') => {
                    chars.next();
                    chars.next();
                }
                _ => {}
            },
            // (?P<Name>..), (?<Name>..) 그룹명과 (?U) 등 플래그
            '(' if chars.peek() == Some(&'?') => {
                chars.next();
                if chars.peek() == Some(&'P') {
                    chars.next();
                }
                if chars.peek() == Some(&'<') {
                    chars.next();
                    // lookbehind 제외
                    if !matches!(chars.peek(), Some('=' | '!')) {
                        for inner in chars.by_ref() {
                            if inner == '>' {
                                break;
                            }
                        }
                    }
                }
                while chars.peek().is_some_and(|flag| "imsUuxR-".contains(*flag)) {
                    chars.next();
                }
            }
            _ if ch.is_uppercase() => return true,
            _ => {}
        }
    }
    false
}

// 2a. regex(bytes) -> grep Matcher adapter -----------------------------------------
// rg와 동일하게 라인 지향 ^/$ 매칭을 위해 (?m)을 상시 켠다. 기본에서는 `.`이 개행을
// 매치하지 않아 매치가 한 줄 안에 갇히고, multiline 옵션이 (?s)로 줄 경계를 연다.
#[derive(Clone, Debug)]
pub(crate) struct RegexAdapter {
    pub(crate) regex: regex::bytes::Regex,
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
// linear engine이 문법으로만 인지하고 실행은 못 하는 lookaround/backreference 전용 backtracking
// 어댑터. find_from_pos는 pos 이전 텍스트를 lookbehind로 참조하므로 grep의 재개(find_at) 계약과
// 정확히 맞고, 비UTF-8 조각은 매치 없음으로 흘려보낸다(binary_detection이 NUL에서 이미 중단).
#[derive(Clone)]
pub(crate) struct FancyAdapter {
    pub(crate) regex: fancy_regex::Regex,
}
impl Matcher for FancyAdapter {
    type Captures = NoCaptures;
    type Error = NoError;
    fn find_at(&self, haystack: &[u8], at: usize) -> Result<Option<Match>, NoError> {
        let Ok(text) = std::str::from_utf8(haystack) else {
            return Ok(None);
        };
        // backtrack_limit 초과 등 런타임 오류는 이 조각에서 매치 없음으로 처리(전체 검색 유지).
        match self.regex.find_from_pos(text, at) {
            Ok(Some(found)) => Ok(Some(Match::new(found.start(), found.end()))),
            _ => Ok(None),
        }
    }
    fn new_captures(&self) -> Result<NoCaptures, NoError> {
        Ok(NoCaptures::new())
    }
}
// linear/backtracking 두 엔진을 한 검색 경로로 흘려보내는 래퍼. 대다수는 linear(빠름)로 가고
// lookaround/backreference를 쓰는 패턴만 fancy(backtracking)로 라우팅된다.
#[derive(Clone)]
pub(crate) enum SearchMatcher {
    Linear(RegexAdapter),
    Fancy(FancyAdapter),
}
impl Matcher for SearchMatcher {
    type Captures = NoCaptures;
    type Error = NoError;
    fn find_at(&self, haystack: &[u8], at: usize) -> Result<Option<Match>, NoError> {
        match self {
            SearchMatcher::Linear(matcher) => matcher.find_at(haystack, at),
            SearchMatcher::Fancy(matcher) => matcher.find_at(haystack, at),
        }
    }
    fn new_captures(&self) -> Result<NoCaptures, NoError> {
        Ok(NoCaptures::new())
    }
}
// 패턴 컴파일 옵션: literal=rg -F, word_match=rg -w, multiline=rg -U(+dot-all) 대응.
struct PatternOpts {
    ignore_case: bool,
    literal: bool,
    multiline: bool,
    word_match: bool,
}
fn build_matcher(
    pattern: &str,
    opts: &PatternOpts,
    force_literal: bool,
) -> Result<RegexAdapter, regex::Error> {
    let mut source = if opts.literal || force_literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    if opts.word_match {
        // rg -w 계약: 패턴 앞뒤에 \b. Rust regex의 \b는 Unicode-aware라 한글 경계도 성립.
        source = format!(r"\b(?:{source})\b");
    }
    let regex = regex::bytes::RegexBuilder::new(&source)
        .case_insensitive(opts.ignore_case)
        .multi_line(true)
        .dot_matches_new_line(opts.multiline)
        .build()?;
    Ok(RegexAdapter { regex })
}
// backtracking 엔진(fancy-regex) 컴파일: lookaround/backreference 지원. RegexBuilder에 플래그
// 세터가 없어 옵션은 인라인 플래그로 반영한다((?m) 상시, (?i) ignore_case, (?s) multiline dot-all).
// backtrack_limit로 병리적 패턴의 폭주를 막고(초과 조각은 매치 없음 처리), 검색 timeout이 상한.
fn build_fancy_matcher(
    pattern: &str,
    opts: &PatternOpts,
) -> Result<FancyAdapter, fancy_regex::Error> {
    let mut source = if opts.word_match {
        // rg -w 계약: 패턴 앞뒤에 \b. fancy-regex의 \b도 Unicode-aware라 한글 경계도 성립.
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern.to_string()
    };
    let mut flags = String::from("m");
    if opts.ignore_case {
        flags.push('i');
    }
    if opts.multiline {
        flags.push('s');
    }
    source = format!("(?{flags}){source}");
    let regex = fancy_regex::RegexBuilder::new(&source)
        .backtrack_limit(1_000_000)
        .build()?;
    Ok(FancyAdapter { regex })
}
// linear engine(Rust regex)이 문법으로는 인지하지만 실행하지 못하는 구문 판별. 이때의 리터럴
// 폴백은 패턴 텍스트 자체를 찾는 오검색이라 대신 fancy 라우팅 또는 크기 초과 거절로 분기한다.
enum Unsupported {
    Fancy,
    TooBig(usize),
}
fn classify_unsupported(error: &regex::Error) -> Option<Unsupported> {
    if let regex::Error::CompiledTooBig(limit) = error {
        return Some(Unsupported::TooBig(*limit));
    }
    let text = error.to_string();
    if text.contains("look-around") || text.contains("backreference") {
        return Some(Unsupported::Fancy);
    }
    None
}
// regex 파스 오류는 캐럿 포함 다중 줄이라 마지막 "error: ..." 요지만 라벨에 싣는다.
fn parse_error_gist(error: &regex::Error) -> String {
    let text = error.to_string();
    text.lines()
        .rev()
        .find_map(|line| line.strip_prefix("error: "))
        .unwrap_or("regex parse error")
        .to_string()
}

// 2b. Parallel walk + per-file sink ---------------------------------------------
struct NativeOutcome {
    lines: Vec<String>,
    timed_out: bool,
    partial: bool,
    limit_reached: bool,
}
// 파일 단위 그룹 (경로순 정렬)
struct Collected {
    groups: Vec<(PathBuf, Vec<String>)>,
    bytes: usize,
}
// 프로브 상한: 이 개수나 PROBE_LIMIT 안에 트리가 끝나면 단일 스레드로 충분하다.
// 실측(2026-10, Windows) 단일 스레드 스캔은 40MB/2000파일에서 58ms, 8스레드 18ms이고
// 200파일/1.6MB 이하에서는 스레드 기동 비용 때문에 단일 스레드가 더 빠르다.
const WALK_PROBE_ENTRIES: usize = 256;
const WALK_PROBE_LIMIT_MS: u64 = 2;
// filePattern / 기본 제외는 rg --glob과 동일한 overrides 시맨틱으로 적용한다.
fn walk_builder(spec: &SearchSpec) -> Result<ignore::WalkBuilder, String> {
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
            overrides
                .add("!**/.git/**")
                .map_err(|error| error.to_string())?;
        }
    }
    let overrides = overrides.build().map_err(|error| error.to_string())?;

    // hidden 스킵, 저장소 .gitignore 만 존중, 심링크 미추적.
    // global excludes·info/exclude·.ignore 제외 (호출당 고정 탐색 비용 약 7ms 실측)
    let mut builder = ignore::WalkBuilder::new(&spec.root);
    builder
        .hidden(!spec.include_hidden)
        .ignore(false)
        .git_ignore(spec.respect_ignore)
        .git_global(false)
        .git_exclude(false)
        .parents(spec.respect_ignore)
        .follow_links(false)
        .overrides(overrides);
    Ok(builder)
}
// 워커 기동은 호출당 약 0.5ms/스레드(실측)이고 대형 트리에서만 회수된다. 그래서 루트가
// 파일이면 상한을 프로브하지 않고 바로 단일 스레드로, 디렉터리면 상한까지 걸어 보고 정한다.
fn search_threads(spec: &SearchSpec) -> usize {
    let file_root = std::fs::metadata(&spec.root)
        .map(|meta| meta.is_file())
        .unwrap_or(false);
    if file_root {
        return 1;
    }
    if walk_entry_probe(&spec.root) >= WALK_PROBE_ENTRIES {
        return available_parallelism().clamp(2, 12);
    }
    1
}
// 디렉터리 엔트리만 세는 규모 추정. gitignore·overrides 필터를 태우지 않아 스캔보다
// 1~2 자릿수 싸고, 상한을 넘으면 즉시 "큰 트리"로 판정한다.
fn walk_entry_probe(root: &std::path::Path) -> usize {
    let started = Instant::now();
    let mut count = 0usize;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            count += 1;
            // 상한 도달은 물론, 시간 상한 초과도 "큰 트리"로 본다. 프로브가 느렸다는 사실 자체가
            // 단일 스레드로는 더 느려진다는 뜻이므로 여기서 작다고 판정하면 안 된다.
            if count >= WALK_PROBE_ENTRIES
                || started.elapsed() >= Duration::from_millis(WALK_PROBE_LIMIT_MS)
            {
                return WALK_PROBE_ENTRIES;
            }
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                pending.push(entry.path());
            }
        }
    }
    count
}
fn run_native_search(spec: &SearchSpec, matcher: &SearchMatcher) -> Result<NativeOutcome, String> {
    let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms);
    let collected = Mutex::new(Collected {
        groups: Vec::new(),
        bytes: 0,
    });
    // hits 는 파일당 예산 스냅샷을 lock-free 로 읽도록 Mutex 밖 atomic 으로 분리(파일마다 lock 제거).
    let hits = AtomicUsize::new(0);
    let emitted_bytes = AtomicUsize::new(0);
    let timed_out = AtomicBool::new(false);
    let partial = AtomicBool::new(false);
    let done = AtomicBool::new(false);

    let mut builder = walk_builder(spec)?;
    builder.threads(search_threads(spec));
    builder.build_parallel().run(|| {
        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .before_context(spec.context)
            .after_context(spec.context)
            .multi_line(spec.multiline)
            .binary_detection(BinaryDetection::quit(0))
            .encoding(spec.encoding.clone())
            .build();
        let matcher = matcher.clone();
        let collected = &collected;
        let hits = &hits;
        let emitted_bytes = &emitted_bytes;
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
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                return WalkState::Continue;
            }
            // 남은 전역 예산 스냅샷(lock-free): 파일 안에서는 이 한도까지만 수집하고 병합 시 재절단한다.
            let budget = {
                let current = hits.load(Ordering::Relaxed);
                if current >= spec.max_results {
                    done.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
                spec.max_results - current
            };
            let byte_room = spec
                .byte_budget
                .saturating_sub(emitted_bytes.load(Ordering::Relaxed));
            if byte_room == 0 {
                done.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let mut sink = FileSink {
                lines: Vec::new(),
                hits: 0,
                budget,
                bytes: 0,
                byte_room,
                max_columns: spec.max_columns,
                matcher: &matcher,
                deadline,
                timed_out,
                tick: 0,
            };
            let search = std::fs::File::open(entry.path()).and_then(|file| {
                let reader = SearchReader {
                    reader: file,
                    deadline,
                    timed_out,
                    done,
                };
                searcher.search_reader(&matcher, reader, &mut sink)
            });
            let stopped = timed_out.load(Ordering::Relaxed) || done.load(Ordering::Relaxed);
            if search.is_err() && !stopped {
                // 읽기 실패(잠긴 파일 등)는 부분 결과로 계속한다(rg 종료코드 2 대응).
                partial.store(true, Ordering::Relaxed);
                return WalkState::Continue;
            }
            // 타임아웃 중이던 파일의 수집분도 병합 후 종료
            if sink.lines.is_empty() {
                return if stopped {
                    WalkState::Quit
                } else {
                    WalkState::Continue
                };
            }
            let mut collected = collected.lock().unwrap();
            // hits 읽기·갱신을 lock 구간 안에서 수행해 정확한 max_results 컷 유지(예산 스냅샷만 lock-free).
            let current = hits.load(Ordering::Relaxed);
            let remaining = spec.max_results.saturating_sub(current);
            if remaining == 0 || collected.bytes >= spec.byte_budget {
                done.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let available = sink.lines.len();
            let mut lines = std::mem::take(&mut sink.lines);
            lines.truncate(remaining);
            // 파일 단위 잔여 바이트 예산 채움
            let heading_bytes = entry.path().as_os_str().len() + 1;
            let room = spec.byte_budget - collected.bytes;
            let mut used = heading_bytes;
            let fit = lines
                .iter()
                .position(|line| {
                    used += line.len() + 1;
                    used > room
                })
                .unwrap_or(lines.len());
            lines.truncate(fit);
            let kept = lines.len();
            if kept > 0 {
                let used = heading_bytes + lines.iter().map(|line| line.len() + 1).sum::<usize>();
                collected.groups.push((entry.path().to_path_buf(), lines));
                collected.bytes += used;
                emitted_bytes.store(collected.bytes, Ordering::Relaxed);
            }
            hits.store(current + kept, Ordering::Relaxed);
            if timed_out.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            // 예산 소진·부분 수집 시 종료
            let sink_clipped = sink.hits >= sink.budget || sink.bytes >= sink.byte_room;
            if current + kept >= spec.max_results
                || collected.bytes >= spec.byte_budget
                || kept < available
                || sink_clipped
            {
                done.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            WalkState::Continue
        })
    });

    let mut collected = collected
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let hit_cap = done.into_inner();
    collected.groups.sort_by(|left, right| left.0.cmp(&right.0));
    let mut lines =
        Vec::with_capacity(collected.groups.iter().map(|group| group.1.len() + 1).sum());
    for (path, group) in collected.groups {
        // Emit the file path once per file run (rg --heading style).
        lines.push(path.display().to_string());
        lines.extend(group);
    }
    Ok(NativeOutcome {
        lines,
        timed_out: timed_out.into_inner() && !hit_cap,
        partial: partial.into_inner(),
        limit_reached: hit_cap,
    })
}
// Search read cancellation
struct SearchReader<'a, R> {
    reader: R,
    deadline: Instant,
    timed_out: &'a AtomicBool,
    done: &'a AtomicBool,
}
impl<R: Read> Read for SearchReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.done.load(Ordering::Relaxed) {
            return Ok(0);
        }
        if self.timed_out.load(Ordering::Relaxed) || Instant::now() >= self.deadline {
            self.timed_out.store(true, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "search time budget exceeded",
            ));
        }
        let chunk_len = buffer.len().min(64 * 1024);
        let read_bytes = self.reader.read(&mut buffer[..chunk_len])?;
        if Instant::now() >= self.deadline {
            self.timed_out.store(true, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "search time budget exceeded",
            ));
        }
        Ok(read_bytes)
    }
}
// 파일 1개 분량의 매치/컨텍스트를 "N:text" / "N-text"로 수집하는 sink.
struct FileSink<'a> {
    lines: Vec<String>,
    hits: usize,
    budget: usize,
    bytes: usize,
    byte_room: usize,
    max_columns: usize,
    matcher: &'a SearchMatcher,
    deadline: Instant,
    timed_out: &'a AtomicBool,
    tick: u32,
}
impl FileSink<'_> {
    fn push_line(
        &mut self,
        number: Option<u64>,
        sep: char,
        bytes: &[u8],
    ) -> Result<bool, std::io::Error> {
        // 시계 호출은 64회에 1회만: 핫루프에서 Instant::now()가 줄당 비용을 지배하지 않게.
        self.tick = self.tick.wrapping_add(1);
        if self.tick & 63 == 0 && Instant::now() >= self.deadline {
            self.timed_out.store(true, Ordering::Relaxed);
            return Ok(false);
        }
        // 비UTF-8 줄 EUC-KR 디코딩
        let (text, _) = decode_line(bytes);
        let text = text.trim_end_matches(['\r', '\n']);
        // multiline 매치는 한 SinkMatch에 여러 줄이 실려 온다: 줄별로 실제 줄번호를 잇는다.
        for (number, line) in (number.unwrap_or(0)..).zip(text.split('\n')) {
            let line = line.trim_end_matches('\r');
            let line = clip_line(line, sep == ':', self.matcher, self.max_columns);
            let entry = format!("{number}{sep}{line}");
            self.bytes += entry.len() + 1;
            self.lines.push(entry);
            self.hits += 1;
            if self.hits >= self.budget || self.bytes >= self.byte_room {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
// 긴 줄 매치 주변 절단
fn clip_line<'a>(
    line: &'a str,
    is_match: bool,
    matcher: &SearchMatcher,
    max_columns: usize,
) -> Cow<'a, str> {
    if line.len() <= max_columns {
        return Cow::Borrowed(line);
    }
    let total_chars = line.chars().count();
    if total_chars <= max_columns {
        return Cow::Borrowed(line);
    }
    let match_byte = if is_match {
        matcher
            .find_at(line.as_bytes(), 0)
            .ok()
            .flatten()
            .map_or(0, |found| found.start())
    } else {
        0
    };
    let match_char = line
        .char_indices()
        .take_while(|(index, _)| *index < match_byte)
        .count();
    let start_char = match_char
        .saturating_sub(max_columns / 4)
        .min(total_chars - max_columns);
    let window: String = line.chars().skip(start_char).take(max_columns).collect();
    let lead = if start_char > 0 { "…" } else { "" };
    let tail = if start_char + max_columns < total_chars {
        "…"
    } else {
        ""
    };
    Cow::Owned(format!(
        "{lead}{window}{tail} [line clipped: {total_chars} chars]"
    ))
}
impl Sink for FileSink<'_> {
    type Error = std::io::Error;
    fn matched(
        &mut self,
        _searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        self.push_line(mat.line_number(), ':', mat.bytes())
    }
    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, std::io::Error> {
        self.push_line(ctx.line_number(), '-', ctx.bytes())
    }
}

// 3. Shared helpers -------------------------------------------------------------
// 검색 루트가 기본 제외 대상 디렉터리 내부인지 판별(명시 탐색 의도는 존중). dir-list도 공유.
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
    fn stop_search_without_matches_at_deadline() {
        struct SlowSource;
        impl Read for SlowSource {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                std::thread::sleep(Duration::from_millis(20));
                buffer.fill(b'x');
                Ok(buffer.len())
            }
        }
        let matcher = SearchMatcher::Linear(RegexAdapter {
            regex: regex::bytes::Regex::new("needle").unwrap(),
        });
        let timed_out = AtomicBool::new(false);
        let done = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_millis(5);
        let reader = SearchReader {
            reader: SlowSource,
            deadline,
            timed_out: &timed_out,
            done: &done,
        };
        let mut sink = FileSink {
            lines: Vec::new(),
            hits: 0,
            budget: 10,
            bytes: 0,
            byte_room: usize::MAX,
            max_columns: DEFAULT_MAX_COLUMNS,
            matcher: &matcher,
            deadline,
            timed_out: &timed_out,
            tick: 0,
        };
        let failure = SearcherBuilder::new()
            .build()
            .search_reader(&matcher, reader, &mut sink)
            .unwrap_err();
        assert_eq!(failure.kind(), io::ErrorKind::TimedOut);
        assert!(timed_out.load(Ordering::Relaxed));
        assert!(sink.lines.is_empty());
    }

    #[test]
    fn stop_reader_after_global_match_cap() {
        let timed_out = AtomicBool::new(false);
        let done = AtomicBool::new(true);
        let mut reader = SearchReader {
            reader: io::Cursor::new(b"needle"),
            deadline: Instant::now() + Duration::from_secs(1),
            timed_out: &timed_out,
            done: &done,
        };
        assert_eq!(reader.read(&mut [0u8; 16]).unwrap(), 0);
        assert!(!timed_out.load(Ordering::Relaxed));
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
        // 라벨에 파스 오류 요지가 실려 패턴 교정이 가능해야 한다.
        let backend = first_backend(&result);
        assert!(backend.contains("literal fallback"), "{result:?}");
        assert!(backend.contains("unclosed group"), "{backend}");

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
        std::fs::write(
            dir.join("node_modules").join("pkg").join("dep.js"),
            "needle\n",
        )
        .unwrap();
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
        let body: String = (1..=50)
            .map(|index| format!("needle line {index}\n"))
            .collect();
        std::fs::write(dir.join("many.txt"), body).unwrap();

        let result = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "needle", "maxResults": 5, "contextLines": 0 }]
        }));
        assert!(!result.is_error, "{result:?}");
        let structured = result.structured.clone().unwrap();
        // totalCount = 헤딩 1 + 콘텐츠 줄 5.
        assert_eq!(
            structured["results"][0]["data"]["totalCount"], 6,
            "{structured}"
        );

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

    #[test]
    fn lookaround_and_backreferences_match_via_fancy_engine() {
        let dir = temp_dir("rust-fs-mcp-fancy");
        std::fs::write(
            dir.join("a.txt"),
            "foo bar\nbaz bar\nhello hello\nhi there\n",
        )
        .unwrap();

        // lookbehind: "foo "가 앞선 "bar"만 매치되고 접두 텍스트는 결과에 실리지 않는다.
        let look = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "(?<=foo )bar", "contextLines": 0 }]
        }));
        assert!(!look.is_error, "{look:?}");
        let text = look.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("1:foo bar"), "{text}");
        assert!(!text.contains("2:baz bar"), "{text}");
        assert!(first_backend(&look).contains("fancy"), "{look:?}");

        // 부정형 lookbehind: "foo "가 앞서지 '않는' "bar"만 매치.
        let neg = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "(?<!foo )bar", "contextLines": 0 }]
        }));
        assert!(!neg.is_error, "{neg:?}");
        let text = neg.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("2:baz bar"), "{text}");
        assert!(!text.contains("1:foo bar"), "{text}");

        // backreference: 같은 단어가 반복된 줄만 매치.
        let backref = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": r"\b(\w+) \1\b", "contextLines": 0 }]
        }));
        assert!(!backref.is_error, "{backref:?}");
        let text = backref.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("3:hello hello"), "{text}");
        assert!(!text.contains("hi there"), "{text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn explicit_literal_matches_fixed_string_only() {
        let dir = temp_dir("rust-fs-mcp-literal");
        // "foo.bar"는 유효한 regex이기도 해서 자동 폴백이 없다: literal:true만이 정확한 계약.
        std::fs::write(dir.join("a.txt"), "foo.bar\nfooXbar\n").unwrap();

        let result = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "foo.bar", "literal": true, "contextLines": 0 }]
        }));
        assert!(!result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("1:foo.bar"), "{text}");
        assert!(!text.contains("fooXbar"), "{text}");
        assert!(first_backend(&result).contains("(literal)"), "{result:?}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn word_match_bounds_ascii_and_hangul_words() {
        let dir = temp_dir("rust-fs-mcp-word");
        std::fs::write(
            dir.join("a.txt"),
            "cat\nconcatenate\n가나 이후\n가나다 이후\n",
        )
        .unwrap();

        let ascii = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "cat", "wordMatch": true, "contextLines": 0 }]
        }));
        assert!(!ascii.is_error, "{ascii:?}");
        let text = ascii.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("1:cat"), "{text}");
        assert!(!text.contains("concatenate"), "{text}");

        // Rust regex의 \b는 Unicode-aware: 완성형 음절 경계도 단어 경계로 성립한다.
        let hangul = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "가나", "wordMatch": true, "contextLines": 0 }]
        }));
        assert!(!hangul.is_error, "{hangul:?}");
        let text = hangul.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("3:가나 이후"), "{text}");
        assert!(!text.contains("가나다"), "{text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn multiline_lets_patterns_span_lines() {
        let dir = temp_dir("rust-fs-mcp-multiline");
        std::fs::write(
            dir.join("a.rs"),
            "alpha\nfn demo(\n  arg: u32,\n) -> bool {\nomega\n",
        )
        .unwrap();

        // 기본(라인 지향)에서는 `.`이 개행을 매치하지 않아 줄 경계를 넘는 매치가 없다.
        let single = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": r"fn demo\(.*?\) -> bool", "contextLines": 0 }]
        }));
        assert!(!single.is_error, "{single:?}");
        let text = single.content[0]["text"].as_str().unwrap_or_default();
        assert!(!text.contains("fn demo"), "{text}");

        let multi = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": r"fn demo\(.*?\) -> bool", "multiline": true, "contextLines": 0 }]
        }));
        assert!(!multi.is_error, "{multi:?}");
        let text = multi.content[0]["text"].as_str().unwrap_or_default();
        // 매치가 걸친 줄들이 실제 줄번호로 분해되어 나온다.
        assert!(text.contains("2:fn demo("), "{text}");
        assert!(text.contains("3:  arg: u32,"), "{text}");
        assert!(text.contains("4:) -> bool {"), "{text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_root_is_an_error_not_zero_matches() {
        let missing = std::env::temp_dir().join("rust-fs-mcp-definitely-missing-root");
        let result = handle_fs_search(&json!({
            "items": [{ "path": missing.display().to_string(), "pattern": "x" }]
        }));
        assert!(result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("Path does not exist"), "{text}");
    }

    #[test]
    fn gitignore_is_respected_inside_a_repo_by_default() {
        let dir = temp_dir("rust-fs-mcp-gitignore");
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(init.success());
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::write(dir.join(".gitignore"), "dist/\n").unwrap();
        std::fs::write(dir.join("dist").join("bundle.js"), "needle\n").unwrap();
        std::fs::write(dir.join("app.js"), "needle\n").unwrap();

        let default = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 0 }]
        }));
        let text = default.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("app.js"), "{text}");
        assert!(!text.contains("bundle.js"), "{text}");

        let all = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 0, "respectGitignore": false }]
        }));
        let text = all.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("bundle.js"), "{text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn long_lines_are_clipped_around_the_match() {
        let dir = temp_dir("rust-fs-mcp-clip");
        std::fs::write(
            dir.join("min.js"),
            format!("{}needle{}\n", "a".repeat(100_000), "b".repeat(100_000)),
        )
        .unwrap();
        std::fs::write(dir.join("z.js"), "needle tail\n").unwrap();

        let result = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 0 }]
        }));
        assert!(!result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("needle"), "{text}");
        assert!(text.contains("[line clipped: 200006 chars]"), "{text}");
        assert!(text.contains("1:needle tail"), "{text}");
        assert!(text.len() < 5_000, "{}", text.len());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn smart_case_applies_only_without_explicit_ignore_case() {
        let dir = temp_dir("rust-fs-mcp-smartcase");
        std::fs::write(dir.join("a.txt"), "Needle upper\nneedle lower\n").unwrap();
        let search = |pattern: &str, ignore_case: Option<bool>| {
            let mut item =
                json!({ "path": dir.display().to_string(), "pattern": pattern, "contextLines": 0 });
            if let Some(flag) = ignore_case {
                item["ignoreCase"] = json!(flag);
            }
            let result = handle_fs_search(&json!({ "items": [item] }));
            result.content[0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };
        let lower = search("needle", None);
        assert!(
            lower.contains("1:Needle upper") && lower.contains("2:needle lower"),
            "{lower}"
        );
        let upper = search("Needle", None);
        assert!(
            upper.contains("1:Needle upper") && !upper.contains("needle lower"),
            "{upper}"
        );
        let forced = search("Needle", Some(true));
        assert!(forced.contains("2:needle lower"), "{forced}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn smart_case_ignores_escapes_group_names_and_flags() {
        for pattern in [
            r"\p{Lu}foo",
            r"\pLfoo",
            r"(?P<Name>x)",
            r"(?<Name>x)",
            r"(?U)abc",
            r"\x4F",
            r"\x{4F}",
            r"\Wfoo",
        ] {
            assert!(!has_uppercase_literal(pattern, false), "{pattern}");
        }
        for pattern in ["Foo", r"(?<=A)b", r"[A-Z]x"] {
            assert!(has_uppercase_literal(pattern, false), "{pattern}");
        }
        assert!(has_uppercase_literal(r"foo\Name", true));
    }

    #[test]
    fn output_budget_stops_collection_and_flags_limit_reached() {
        let dir = temp_dir("rust-fs-mcp-budget-stop");
        let body: String = (1..=20_000)
            .map(|index| format!("needle line {index}\n"))
            .collect();
        std::fs::write(dir.join("many.txt"), body).unwrap();

        let result = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 0 }]
        }));
        assert!(!result.is_error, "{result:?}");
        let data = &result.structured.as_ref().unwrap()["results"][0]["data"];
        assert_eq!(data["limitReached"], true, "{data}");
        assert!(data["totalCount"].as_u64().unwrap() < 20_000, "{data}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        assert!(
            text.contains("[stopped at maxResults or the output budget"),
            "{}",
            &text[text.len().saturating_sub(300)..]
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn results_are_sorted_by_path() {
        let dir = temp_dir("rust-fs-mcp-sorted");
        for name in ["c.txt", "a.txt", "b.txt"] {
            std::fs::write(dir.join(name), "needle\n").unwrap();
        }
        let result = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 0 }]
        }));
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        let order: Vec<usize> = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|name| text.find(name).unwrap())
            .collect();
        assert!(order[0] < order[1] && order[1] < order[2], "{text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn euc_kr_files_are_searchable_and_readable() {
        let dir = temp_dir("rust-fs-mcp-euckr-search");
        // "한글 needle" in EUC-KR.
        std::fs::write(
            dir.join("legacy.txt"),
            [
                0xC7, 0xD1, 0xB1, 0xDB, b' ', b'n', b'e', b'e', b'd', b'l', b'e', b'\n',
            ],
        )
        .unwrap();

        let ascii = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 0 }]
        }));
        let text = ascii.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("1:한글 needle"), "{text}");

        let hangul = handle_fs_search(&json!({
            "items": [{ "path": dir.display().to_string(), "pattern": "한글", "contextLines": 0, "encoding": "euc-kr" }]
        }));
        assert!(!hangul.is_error, "{hangul:?}");
        let text = hangul.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("1:한글 needle"), "{text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
