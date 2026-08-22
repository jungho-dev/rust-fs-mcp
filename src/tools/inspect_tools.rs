//! inspect_tools.rs
//! tools::inspect_tools
//!
//! Compact read-only filesystem inspection (fs-inspect) tool aimed at coding workflows.
//! Bundles count-files / search / json-pick / snippet modes into one batched call.
//!

use crate::core::batch::{available_parallelism, pool_execute};
use crate::core::config::ensure_path_allowed;
use crate::core::response::RawResult;
use crate::tools::search_tools::{RegexAdapter, path_in_heavy_dir};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

// inspect_tools' wildcard_match applies the same pattern to many entries repeatedly.
// Re-running Regex::new on every call would blow up compile cost and allocations, so
// compiled Regex values are cached per pattern. Mirrors search_tools' GLOB_CACHE pattern.
static INSPECT_WILDCARD_CACHE: OnceLock<RwLock<HashMap<String, Regex>>> = OnceLock::new();
static EXTRACT_REGEX_CACHE: OnceLock<RwLock<HashMap<String, Regex>>> = OnceLock::new();

struct InspectState {
    max_chars: usize,
    used_chars: usize,
    scanned_files: usize,
    bytes_read: usize,
    truncated: bool,
    deadline: Instant,
    budget_hit: bool,
    budget_tick: u32,
}
// 요청 단위 수집 메트릭: 병렬 실행 후 병합해 응답 metrics로 내보낸다.
type InspectOutcome = (Value, InspectMetrics);
type InspectSlots = Arc<Vec<Mutex<Option<InspectOutcome>>>>;
#[derive(Clone, Default)]
struct InspectMetrics {
    scanned_files: usize,
    bytes_read: usize,
    snippet_chars: usize,
    truncated: bool,
    budget_hit: bool,
}
impl InspectMetrics {
    fn merge(&mut self, other: &InspectMetrics) {
        self.scanned_files += other.scanned_files;
        self.bytes_read += other.bytes_read;
        self.snippet_chars += other.snippet_chars;
        self.truncated |= other.truncated;
        self.budget_hit |= other.budget_hit;
    }
}
// Codex 계열 클라이언트의 tools/call 30초 상한 안쪽에서 스스로 마감해 부분 결과를 돌려준다.
fn inspect_budget_ms() -> u64 {
    25_000
}
fn budget_exceeded(state: &mut InspectState) -> bool {
    if state.budget_hit {
        return true;
    }
    // 시계 호출은 128회에 한 번만(go budgetTick&127 동일) — 순회 핫루프에서 Instant::now()가
    // 항목당 시간을 지배하던 비용을 제거한다.
    state.budget_tick = state.budget_tick.wrapping_add(1);
    if state.budget_tick & 127 != 0 {
        return false;
    }
    if Instant::now() >= state.deadline {
        state.budget_hit = true;
        state.truncated = true;
    }
    state.budget_hit
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
// fs-search와 동일한 기본 제외 상태: .git은 항상, 대형 산출물 디렉터리(node_modules/target)는
// 시작 경로가 그 내부가 아닐 때만 건너뛴다(대형 트리 시간 예산 소진 방지).
// 요청이 noDefaultExcludes:true면 전부 순회한다.
#[derive(Clone, Copy)]
struct DirExcludes {
    enabled: bool,
    skip_heavy: bool,
}
impl DirExcludes {
    fn from_request(request: &Value, path: &Path) -> Self {
        Self {
            enabled: !bool_field(request, "noDefaultExcludes", false),
            skip_heavy: !path_in_heavy_dir(path),
        }
    }
    fn skips(&self, name: &str) -> bool {
        if !self.enabled {
            return false;
        }
        if name == ".git" {
            return true;
        }
        self.skip_heavy && (name == "node_modules" || name == "target")
    }
}
// 수집(순회) 컨텍스트: 파일 내용 스캔 자체는 ScanCtx가 담당한다.
struct SearchCtx<'a> {
    root: &'a Path,
    recursive: bool,
    file_pattern: Option<&'a str>,
    excludes: DirExcludes,
}
// 파일 스캔 컨텍스트: 병렬 워커로 이동 가능하도록 소유 데이터만 담는다.
struct ScanCtx {
    root: PathBuf,
    matcher: RegexAdapter,
    extracts: Vec<ExtractSpec>,
    max_matches: usize,
    deadline: Instant,
}
// 파일 1개의 병렬 스캔 결과: 수집 순서대로 병합되어 순차 실행과 같은 결과가 된다.
struct FileScan {
    hits: Vec<Hit>,
    warnings: Vec<String>,
    scanned: usize,
    bytes: usize,
    budget_hit: bool,
}
// 1. FS inspect tool ----------------------------------------------------------
pub fn handle_fs_inspect(args: &Value) -> RawResult {
    let Some(root_text) = args.get("root").and_then(Value::as_str) else {
        return RawResult::error(
            "root must be a string (fs-inspect args are {root, requests:[{op, path, ...}]}, not items[])",
        );
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
        .unwrap_or(6000);
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("strict");
    let deadline = Instant::now() + Duration::from_millis(inspect_budget_ms());
    let total = requests.len();
    // 요청들은 서로 독립이므로 상주 풀에서 병렬 실행(go runInspectRequests 동일).
    // 각 요청은 자체 state(요청별 evidence 예산)로 수집하고, 종료 후 전역 예산을
    // 답변 순서대로 재적용한다.
    let workers = total.min(available_parallelism()).max(1);
    let outcomes: Vec<(Value, InspectMetrics)> = if total <= 1 || workers <= 1 {
        requests
            .iter()
            .enumerate()
            .map(|(index, request)| run_request_owned(&root, request, index + 1, max_chars, deadline))
            .collect()
    } else {
        let shared_requests = Arc::new(requests.clone());
        let shared_root = Arc::new(root.clone());
        let slots: InspectSlots = Arc::new((0..total).map(|_| Mutex::new(None)).collect());
        let job_requests = Arc::clone(&shared_requests);
        let job_root = Arc::clone(&shared_root);
        let job_slots = Arc::clone(&slots);
        pool_execute(total, workers - 1, move |index| {
            let outcome =
                run_request_owned(&job_root, &job_requests[index], index + 1, max_chars, deadline);
            *job_slots[index].lock().unwrap() = Some(outcome);
        });
        slots
            .iter()
            .map(|slot| {
                slot.lock().unwrap().take().unwrap_or_else(|| {
                    (
                        answer_error("request", "unknown", "inspect worker panicked"),
                        InspectMetrics::default(),
                    )
                })
            })
            .collect()
    };
    let mut metrics = InspectMetrics::default();
    for (_, request_metrics) in &outcomes {
        metrics.merge(request_metrics);
    }
    let mut answers: Vec<Value> = outcomes.into_iter().map(|(answer, _)| answer).collect();
    apply_evidence_budget(&mut answers, max_chars, &mut metrics);
    let status = inspect_status(&answers);
    let text = format!(
        "fs-inspect: {} requests, status={status}, snippetChars={}",
        answers.len(),
        metrics.snippet_chars
    );

    RawResult::structured(
        text,
        json!({
            "status": status,
            "mode": mode,
            "answers": answers,
            "metrics": {
                "scannedFiles": metrics.scanned_files,
                "bytesRead": metrics.bytes_read,
                "snippetChars": metrics.snippet_chars,
                "truncated": metrics.truncated,
                "timeBudgetHit": metrics.budget_hit
            }
        }),
    )
}
// 요청 1건을 자체 state로 실행하고 (answer, metrics)로 반환한다.
fn run_request_owned(
    root: &Path,
    request: &Value,
    index: usize,
    max_chars: usize,
    deadline: Instant,
) -> (Value, InspectMetrics) {
    let mut state = InspectState {
        max_chars,
        used_chars: 0,
        scanned_files: 0,
        bytes_read: 0,
        truncated: false,
        deadline,
        budget_hit: false,
        budget_tick: 0,
    };
    let answer = run_request(root, request, index, &mut state);
    let metrics = InspectMetrics {
        scanned_files: state.scanned_files,
        bytes_read: state.bytes_read,
        snippet_chars: state.used_chars,
        truncated: state.truncated,
        budget_hit: state.budget_hit,
    };
    (answer, metrics)
}
// 전역 evidence 예산(go applyInspectEvidenceBudget 대응): 수집은 요청별 예산으로 했으므로
// 최종 응답이 maxSnippetChars를 넘지 않도록 답변 순서대로 rune 단위 재절단한다.
fn apply_evidence_budget(answers: &mut [Value], max_chars: usize, metrics: &mut InspectMetrics) {
    let mut remaining = max_chars;
    let mut kept = 0usize;
    for answer in answers.iter_mut() {
        let Some(evidence) = answer.get_mut("evidence").and_then(Value::as_array_mut) else {
            continue;
        };
        for entry in evidence.iter_mut() {
            let Some(snippet) = entry.get("snippet").and_then(Value::as_str) else {
                continue;
            };
            // ASCII면 chars().count() 스캔 생략.
            let count = if snippet.is_ascii() { snippet.len() } else { snippet.chars().count() };
            if count <= remaining {
                remaining -= count;
                kept += count;
                continue;
            }
            let truncated: String = snippet.chars().take(remaining).collect();
            // take(remaining)은 정확히 remaining자를 만들므로 재카운트 스캔을 생략한다.
            kept += remaining;
            entry["snippet"] = Value::String(truncated);
            remaining = 0;
            metrics.truncated = true;
        }
    }
    metrics.snippet_chars = kept;
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
        }
        else {
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
    let excludes = DirExcludes::from_request(request, &path);
    let mut samples = Vec::new();
    let count = match count_dir(root, &path, glob, recursive, excludes, &mut samples, state) {
        Ok(count) => count,
        Err(error) => return answer_error(id, op, error),
    };
    let mut evidence = Vec::new();
    let sample_text = if samples.is_empty() {
        "no matched files".to_string()
    }
    else {
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

    let value = json!({
        "path": rel_path(root, &path),
        "glob": glob,
        "recursive": recursive,
        "count": count
    });
    if state.budget_hit {
        return answer_partial(
            id,
            op,
            value,
            "medium",
            evidence,
            vec!["time budget exceeded; count is partial".to_string()],
        );
    }
    answer_ok(id, op, value, "high", evidence, Vec::new())
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
        excludes: DirExcludes::from_request(request, &path),
    };
    // 1) 대상 파일을 경로 정렬 순서로 수집하고 2) 파일 단위로 병렬 스캔한 뒤 3) 수집
    // 순서대로 병합한다: 순차 실행과 동일한 결과를 유지하면서 read+regex가 코어를 나눠 쓴다.
    let mut files = Vec::new();
    if let Err(error) = collect_search_files(&ctx, &path, &mut files, &mut warnings, state) {
        return answer_error(id, op, error);
    }
    let scan = ScanCtx {
        root: root.to_path_buf(),
        matcher,
        extracts,
        max_matches,
        deadline: state.deadline,
    };
    scan_files(scan, files, &mut hits, &mut warnings, state);
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
        }
        else {
            warnings.push(format!("missing pointer: {pointer}"));
        }
    }
    let status = if warnings.is_empty() { "ok" } else { "partial" };
    let confidence = if warnings.is_empty() {
        "high"
    }
    else {
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

    // 패턴별 contains 순회는 O(라인수 × 패턴수)이므로, 리터럴 이스케이프 대안을 합성한
    // 단일 regex로 라인당 1회 검사한다(패턴 1개 또는 합성 실패 시 contains 순회로 폴백).
    let matcher = combined_literal_matcher(&patterns);
    let lines = text.lines().collect::<Vec<_>>();
    let mut ranges = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let hit = match &matcher {
            Some(regex) => regex.is_match(line),
            None => patterns.iter().any(|pattern| line.contains(pattern)),
        };
        if !hit {
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
        // 중간 Vec + join 대신 단일 String에 직접 누적한다.
        let mut snippet = String::new();
        for (index, line) in lines.iter().enumerate().take(*end).skip(*start) {
            if !snippet.is_empty() {
                snippet.push('\n');
            }
            let _ = write!(snippet, "{}: {}", index + 1, line);
        }
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
    }
    else {
        root.join(raw)
    };
    let path = ensure_path_allowed(joined)?;
    // exists() 선행 검사는 같은 정보를 주는 canonicalize와 syscall이 중복되므로 합친다.
    // NotFound는 기존과 동일한 "Path does not exist" 메시지로 매핑해 오류 계약을 유지한다.
    let path = fs::canonicalize(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("Path does not exist: {}", path.display())
        }
        else {
            format!("Failed to canonicalize {}: {error}", path.display())
        }
    })?;
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
    excludes: DirExcludes,
    samples: &mut Vec<String>,
    state: &mut InspectState,
) -> Result<usize, String> {
    let mut count = 0;
    for entry in read_dir(dir)? {
        // 시간 예산 초과 시 순회를 멈추고 지금까지의 카운트를 부분 결과로 반환.
        if budget_exceeded(state) {
            break;
        }
        // 디렉터리 리스팅이 준 file_type을 재사용해 항목당 is_dir/is_file metadata 조회를
        // 없애고, 심링크/정션은 순환 재귀(스택 오버플로)를 유발하므로 따라가지 않음.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            let path = entry.path();
            let name = path.file_name().and_then(|value| value.to_str()).unwrap_or("");
            if recursive && !excludes.skips(name) {
                count += count_dir(root, &path, glob, recursive, excludes, samples, state)?;
            }
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        state.scanned_files += 1;
        // 파일 항목은 PathBuf 할당(entry.path()) 없이 파일명으로 바로 판정한다.
        let name = entry.file_name();
        if !wildcard_match(glob, name.to_str().unwrap_or("")) {
            continue;
        }
        count += 1;
        if samples.len() < 20 {
            samples.push(rel_path(root, &entry.path()));
        }
    }
    Ok(count)
}
// 대상 파일을 경로 정렬 순서로 수집한다(제외 규칙/심링크/파일 필터/시간 예산 동일 적용).
fn collect_search_files(
    ctx: &SearchCtx<'_>,
    path: &Path,
    files: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
    state: &mut InspectState,
) -> Result<(), String> {
    if path.is_file() {
        if file_pattern_hits(ctx, path) {
            files.push(path.to_path_buf());
        }
        return Ok(());
    }
    if !path.is_dir() {
        return Err(format!(
            "Path is not a file or directory: {}",
            path.display()
        ));
    }
    for entry in read_dir(path)? {
        if budget_exceeded(state) {
            push_budget_warning(warnings);
            return Ok(());
        }
        // 디렉터리 리스팅이 준 file_type을 재사용해 항목당 is_dir/is_file metadata 조회를
        // 없애고, 심링크/정션은 순환 재귀(스택 오버플로)를 유발하므로 따라가지 않음.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            let child = entry.path();
            let name = child.file_name().and_then(|value| value.to_str()).unwrap_or("");
            if ctx.recursive && !ctx.excludes.skips(name) {
                collect_search_files(ctx, &child, files, warnings, state)?;
            }
        }
        else if kind.is_file() {
            let child = entry.path();
            if file_pattern_hits(ctx, &child) {
                files.push(child);
            }
        }
    }
    Ok(())
}
fn file_pattern_hits(ctx: &SearchCtx<'_>, path: &Path) -> bool {
    let Some(pattern) = ctx.file_pattern else {
        return true;
    };
    let name = path.file_name().and_then(|value| value.to_str()).unwrap_or("");
    wildcard_match(pattern, name) || wildcard_match(pattern, &rel_path(ctx.root, path))
}
fn push_budget_warning(warnings: &mut Vec<String>) {
    if !warnings.iter().any(|warning| warning.starts_with("time budget")) {
        warnings.push("time budget exceeded; matches are partial".to_string());
    }
}
// 수집된 파일들을 스캔한다: 소배치는 순차, 그 외에는 상주 풀에서 파일 단위 병렬.
// 병합이 수집(경로 정렬) 순서를 따르므로 결과는 순차 실행과 동일하다.
fn scan_files(
    scan: ScanCtx,
    files: Vec<PathBuf>,
    hits: &mut Vec<Hit>,
    warnings: &mut Vec<String>,
    state: &mut InspectState,
) {
    let total = files.len();
    if total == 0 {
        return;
    }
    let workers = total.min(available_parallelism()).min(12);
    if total <= 2 || workers <= 1 {
        for file in &files {
            if hits.len() >= scan.max_matches {
                warnings.push("maxMatches reached".to_string());
                return;
            }
            if budget_exceeded(state) {
                push_budget_warning(warnings);
                return;
            }
            with_searcher(|searcher| search_file(&scan, searcher, file, hits, warnings, state));
        }
        return;
    }
    let max_matches = scan.max_matches;
    let slots: Arc<Vec<Mutex<Option<FileScan>>>> =
        Arc::new((0..total).map(|_| Mutex::new(None)).collect());
    let shared = Arc::new((scan, files));
    let job_shared = Arc::clone(&shared);
    let job_slots = Arc::clone(&slots);
    pool_execute(total, workers - 1, move |index| {
        let (scan, files) = &*job_shared;
        let mut out = FileScan {
            hits: Vec::new(),
            warnings: Vec::new(),
            scanned: 0,
            bytes: 0,
            budget_hit: false,
        };
        // 파일 단위 deadline 검사: 예산 소진 후 남은 파일은 읽지 않는다.
        if Instant::now() >= scan.deadline {
            out.budget_hit = true;
        }
        else {
            let mut local = InspectState {
                max_chars: usize::MAX,
                used_chars: 0,
                scanned_files: 0,
                bytes_read: 0,
                truncated: false,
                deadline: scan.deadline,
                budget_hit: false,
                budget_tick: 0,
            };
            with_searcher(|searcher| {
                search_file(scan, searcher, &files[index], &mut out.hits, &mut out.warnings, &mut local);
            });
            out.scanned = local.scanned_files;
            out.bytes = local.bytes_read;
            out.budget_hit = local.budget_hit;
        }
        *job_slots[index].lock().unwrap() = Some(out);
    });
    let mut reached = false;
    for slot in slots.iter() {
        let Some(out) = slot.lock().unwrap().take() else {
            continue;
        };
        state.scanned_files += out.scanned;
        state.bytes_read += out.bytes;
        if out.budget_hit {
            state.budget_hit = true;
            state.truncated = true;
        }
        warnings.extend(out.warnings);
        for hit in out.hits {
            if hits.len() >= max_matches {
                reached = true;
                break;
            }
            hits.push(hit);
        }
    }
    if reached {
        warnings.push("maxMatches reached".to_string());
    }
    if state.budget_hit {
        push_budget_warning(warnings);
    }
}
thread_local! {
    // 상주 풀 스레드가 파일마다 searcher를 다시 만들지 않도록 스레드별로 재사용한다.
    static INSPECT_SEARCHER: RefCell<Searcher> = RefCell::new(line_searcher());
}
// 라인 모드 searcher: 매치가 줄을 넘지 않는 기존 계약을 보존하면서 통버퍼로 스캔한다.
fn line_searcher() -> Searcher {
    SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .build()
}
fn with_searcher(run: impl FnOnce(&mut Searcher)) {
    INSPECT_SEARCHER.with(|cell| run(&mut cell.borrow_mut()));
}
fn search_file(
    scan: &ScanCtx,
    searcher: &mut Searcher,
    path: &Path,
    hits: &mut Vec<Hit>,
    warnings: &mut Vec<String>,
    state: &mut InspectState,
) {
    // 통버퍼 + grep-searcher: 줄 단위 read_line/is_match가 regex 리터럴 prefilter(SIMD
    // memmem)를 무력화하던 병목을 제거한다. NUL 파일은 바이너리로 간주해 건너뛴다.
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) => {
            warnings.push(format!("skipped {}: {error}", rel_path(&scan.root, path)));
            return;
        }
    };
    state.scanned_files += 1;
    state.bytes_read += data.len();
    let mut sink = InspectSink {
        root: &scan.root,
        path,
        rel: None,
        extracts: &scan.extracts,
        max_matches: scan.max_matches,
        hits,
        state,
    };
    if let Err(error) = searcher.search_slice(&scan.matcher, &data, &mut sink) {
        warnings.push(format!("skipped {}: {error}", rel_path(&scan.root, path)));
    }
}
// 파일 1개의 매치 라인을 Hit로 수집하는 sink: rel 경로 문자열은 첫 매치에서만 1회 계산.
struct InspectSink<'a> {
    root: &'a Path,
    path: &'a Path,
    rel: Option<String>,
    extracts: &'a [ExtractSpec],
    max_matches: usize,
    hits: &'a mut Vec<Hit>,
    state: &'a mut InspectState,
}
impl Sink for InspectSink<'_> {
    type Error = std::io::Error;
    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        if self.hits.len() >= self.max_matches || budget_exceeded(self.state) {
            return Ok(false);
        }
        let bytes = mat.bytes();
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        let text = String::from_utf8_lossy(bytes).into_owned();
        let fields = capture_fields(&text, self.extracts);
        let rel = self.rel.get_or_insert_with(|| rel_path(self.root, self.path));
        self.hits.push(Hit {
            rel: rel.clone(),
            line: mat.line_number().unwrap_or(0) as usize,
            text,
            fields,
        });
        Ok(self.hits.len() < self.max_matches)
    }
}
// 9. Search helpers -----------------------------------------------------------
fn search_regex(pattern: &str, literal: bool) -> Result<RegexAdapter, String> {
    let source = if literal {
        regex::escape(pattern)
    }
    else {
        pattern.to_string()
    };
    // (?m)+crlf: 통버퍼 검색에서도 ^/$가 기존 줄 단위 검사(\r\n 제거 후 is_match)와
    // 같은 라인 앵커 의미를 갖게 한다.
    let regex = regex::bytes::RegexBuilder::new(&source)
        .multi_line(true)
        .crlf(true)
        .build()
        .map_err(|error| format!("Invalid search pattern: {error}"))?;
    Ok(RegexAdapter { regex })
}
// snippet 패턴 목록을 단일 regex로 합성한다(각 패턴은 리터럴로 이스케이프).
// 빈 목록은 None을 반환해 "어떤 라인도 매칭하지 않는" 기존 의미를 유지한다.
// 패턴 1개는 str::contains(memchr/SIMD)가 regex 엔진보다 싸므로 합성 생략.
fn combined_literal_matcher(patterns: &[String]) -> Option<Regex> {
    if patterns.len() < 2 {
        return None;
    }
    let source = patterns
        .iter()
        .map(|pattern| regex::escape(pattern))
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&source).ok()
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
        // 동일 패턴은 배치/연속 호출 간 재사용이 자주므로 컴파일 결과를 캐시한다
        // (INSPECT_WILDCARD_CACHE와 같은 패턴; regex::Regex clone은 내부 Arc 공유라 저렴).
        // 조회 결과를 owned 로 받아 읽기 가드를 먼저 닫음: match 스크루티니의 임시 가드는
        // match 종료까지 살아 있어, 미스 분기에서 쓰기 러을 잡으면 자기 교잭임.
        let cache = EXTRACT_REGEX_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
        let cached = cache.read().unwrap().get(pattern).cloned();
        let regex = match cached {
            Some(regex) => regex,
            None => {
                let compiled = Regex::new(pattern)
                    .map_err(|error| format!("Invalid extract regex for {name}: {error}"))?;
                cache
                    .write()
                    .unwrap()
                    .insert(pattern.to_string(), compiled.clone());
                compiled
            }
        };
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
        if let Some(captures) = spec.regex.captures(line) && let Some(value) = captures.get(1)
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
    }
    else {
        // Possible multi-byte content — count exactly and truncate if needed.
        let count = snippet.chars().count();
        if count > remaining {
            state.truncated = true;
            let truncated = truncate_chars(&snippet, remaining);
            // truncate_chars는 정확히 remaining자를 반환하므로 재카운트 스캔을 생략한다.
            (truncated, remaining)
        }
        else {
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
    // 상태 배열을 1회만 순회해 error/partial 개수를 함께 집계한다(기존 3회 순회 대체).
    let mut errors = 0usize;
    let mut partials = 0usize;
    for answer in answers {
        match answer.get("status").and_then(Value::as_str) {
            Some("error") => errors += 1,
            Some("partial") => partials += 1,
            _ => {}
        }
    }
    // 빈 배열 포함: 전부 error이면(all의 공집합 의미 유지) error.
    if errors == answers.len() {
        return "error";
    }
    if errors > 0 || partials > 0 {
        "partial"
    }
    else {
    	"ok"
    }
}
// 12. Small helpers -----------------------------------------------------------
fn read_dir(path: &Path) -> Result<Vec<fs::DirEntry>, String> {
    let mut entries = fs::read_dir(path)
        .map_err(|error| format!("Failed to list {}: {error}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Failed to read directory entry: {error}"))?;
    // 같은 부모 안에서는 파일명 정렬 == 경로 정렬: 비교마다 PathBuf를 할당하던
    // sort_by_key(entry.path()) 대신 파일명 키를 항목당 1회만 만든다.
    entries.sort_by_cached_key(|entry| entry.file_name());
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
    if let Some(last) = ranges.last_mut() && start <= last.1
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
    // 초고빈도 경로: 기본 glob "*"과 단순 prefix/suffix glob은 락/regex 없이 즉시 판정.
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*')
        && !suffix.contains(['*', '?'])
    {
        return text.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*')
        && !prefix.contains(['*', '?'])
    {
        return text.starts_with(prefix);
    }
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
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn search_merges_hits_in_path_order_with_cap() {
        let dir = std::env::temp_dir().join(format!(
            "rust-fs-mcp-inspect-order-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // 파일 12개(병렬 스캔 경로): 각 1매치. cap 5면 경로 정렬상 앞 5개 파일만 남아야 한다.
        for index in 0..12 {
            std::fs::write(
                dir.join(format!("f{index:02}.txt")),
                format!("pad line\nneedle mark {index:02}\n"),
            )
            .unwrap();
        }

        let result = handle_fs_inspect(&json!({
            "root": dir.display().to_string(),
            "requests": [{ "op": "search", "pattern": "needle", "path": ".", "maxMatches": 5 }]
        }));
        assert!(!result.is_error, "{result:?}");
        let structured = result.structured.unwrap();
        let answer = &structured["answers"][0];
        assert_eq!(answer["value"]["matches"], 5, "{answer:?}");
        let evidence = answer["evidence"].as_array().unwrap();
        let paths: Vec<&str> = evidence.iter().map(|entry| entry["path"].as_str().unwrap()).collect();
        assert_eq!(paths, ["f00.txt", "f01.txt", "f02.txt", "f03.txt", "f04.txt"], "{paths:?}");
        assert!(evidence.iter().all(|entry| entry["lineStart"] == 2), "{evidence:?}");
        let warnings = answer["warnings"].as_array().unwrap();
        assert!(warnings.iter().any(|warning| warning == "maxMatches reached"), "{warnings:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_handles_crlf_anchor_and_extract() {
        let dir = std::env::temp_dir().join(format!(
            "rust-fs-mcp-inspect-crlf-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "zero\nneedle one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "alpha\r\nneedle two\r\n").unwrap();

        let result = handle_fs_inspect(&json!({
            "root": dir.display().to_string(),
            "requests": [{
                "op": "search",
                "pattern": r"needle \w+$",
                "path": ".",
                "extract": [{ "name": "word", "regex": r"needle (\w+)" }]
            }]
        }));
        assert!(!result.is_error, "{result:?}");
        let structured = result.structured.unwrap();
        let answer = &structured["answers"][0];
        // CRLF 줄에서도 $ 앵커가 매치(기존 \r\n 제거 후 is_match와 동등)해 두 파일 모두 잡힌다.
        assert_eq!(answer["value"]["matches"], 2, "{answer:?}");
        assert_eq!(answer["value"]["word"], "one", "{answer:?}");
        let evidence = answer["evidence"].as_array().unwrap();
        assert_eq!(evidence[0]["path"], "a.txt", "{evidence:?}");
        assert_eq!(evidence[0]["lineStart"], 2, "{evidence:?}");
        assert_eq!(evidence[1]["path"], "b.txt", "{evidence:?}");
        assert_eq!(evidence[1]["lineStart"], 2, "{evidence:?}");
        // 스니펫에 \r이 남지 않아야 한다.
        assert_eq!(evidence[1]["snippet"], "needle two", "{evidence:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn count_files_matches_glob_recursively() {
        let dir = std::env::temp_dir().join(format!(
            "rust-fs-mcp-inspect-count-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("sub").join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(dir.join("sub").join("c.txt"), "text\n").unwrap();

        let result = handle_fs_inspect(&json!({
            "root": dir.display().to_string(),
            "requests": [{ "op": "count-files", "path": ".", "glob": "*.rs", "recursive": true }]
        }));
        assert!(!result.is_error, "{result:?}");
        let structured = result.structured.unwrap();
        let answer = &structured["answers"][0];
        assert_eq!(answer["value"]["count"], 2, "{answer:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_skips_heavy_dirs_by_default() {
        // 시작 root가 heavy dir 밖이면 node_modules/target 하위는 기본 순회 제외된다.
        // target/ 아래가 아닌 temp_dir에 만들어야 skip_heavy=true 경로를 타며,
        // .git 안의 매치도 기존처럼 제외된다.
        let dir = std::env::temp_dir().join(format!(
            "rust-fs-mcp-inspect-heavy-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("node_modules").join("pkg")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("node_modules").join("pkg").join("dep.js"),
            "needle_here\n",
        )
        .unwrap();
        std::fs::write(dir.join("target").join("out.txt"), "needle_here\n").unwrap();
        std::fs::write(dir.join("src").join("app.js"), "needle_here\n").unwrap();

        let result = handle_fs_inspect(&json!({
            "root": dir.display().to_string(),
            "requests": [{ "op": "search", "pattern": "needle_here", "path": "." }]
        }));
        assert!(!result.is_error, "{result:?}");
        let structured = result.structured.unwrap();
        let answer = &structured["answers"][0];
        assert_eq!(answer["value"]["matches"], 1, "{answer:?}");
        let evidence_path = answer["evidence"][0]["path"].as_str().unwrap_or_default();
        assert!(evidence_path.contains("app.js"), "{evidence_path}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_default_excludes_opens_heavy_and_git_dirs() {
        // noDefaultExcludes:true면 node_modules/target은 물론 상시 제외인 .git까지
        // 요청 단위로 순회에 포함된다.
        let dir = std::env::temp_dir().join(format!(
            "rust-fs-mcp-inspect-noexcl-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("node_modules").join("dep.js"), "needle_here\n").unwrap();
        std::fs::write(dir.join(".git").join("hook.js"), "needle_here\n").unwrap();
        std::fs::write(dir.join("src").join("app.js"), "needle_here\n").unwrap();

        let result = handle_fs_inspect(&json!({
            "root": dir.display().to_string(),
            "requests": [
                { "op": "search", "pattern": "needle_here", "path": ".", "noDefaultExcludes": true, "maxMatches": 10 },
                { "op": "count-files", "path": ".", "glob": "*.js", "recursive": true, "noDefaultExcludes": true }
            ]
        }));
        assert!(!result.is_error, "{result:?}");
        let structured = result.structured.unwrap();
        assert_eq!(structured["answers"][0]["value"]["matches"], 3, "{structured:?}");
        assert_eq!(structured["answers"][1]["value"]["count"], 3, "{structured:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn combined_literal_matcher_skips_single_and_empty_patterns() {
        // 패턴 0~1개는 contains 순회가 더 싸므로 합성 생략.
        assert!(combined_literal_matcher(&[]).is_none());
        assert!(combined_literal_matcher(&["needle".to_string()]).is_none());
        assert!(combined_literal_matcher(&["a".to_string(), "b".to_string()]).is_some());
    }
    #[test]
    fn combined_literal_matcher_treats_patterns_as_literals() {
        // 합성 regex는 메타문자를 이스케이프해 contains 와 동일한 리터럴 의미 유지.
        let matcher = combined_literal_matcher(&["a.c".to_string(), "x+y".to_string()]).unwrap();
        assert!(matcher.is_match("literal a.c here"));
        assert!(matcher.is_match("x+y"));
        assert!(!matcher.is_match("abc"));
        assert!(!matcher.is_match("xy"));
    }
    #[test]
    fn inspect_status_folds_error_and_partial_in_one_pass() {
        assert_eq!(inspect_status(&[json!({ "status": "ok" })]), "ok");
        assert_eq!(
            inspect_status(&[json!({ "status": "ok" }), json!({ "status": "partial" })]),
            "partial"
        );
        assert_eq!(
            inspect_status(&[json!({ "status": "ok" }), json!({ "status": "error" })]),
            "partial"
        );
        assert_eq!(inspect_status(&[json!({ "status": "error" })]), "error");
        // 빈 배열은 기존 all() 공집합 의미대로 error 유지.
        assert_eq!(inspect_status(&[]), "error");
    }
    #[test]
    fn request_path_reports_missing_path_error() {
        // exists() 선행 검사를 canonicalize 로 합친 뒤에도 부재 오류 메시지 계약 동일.
        let root = std::env::temp_dir();
        let request = json!({ "path": "rust-fs-mcp-definitely-missing-probe.txt" });
        let error = request_path(&root, &request).unwrap_err();
        assert!(error.starts_with("Path does not exist:"), "{error}");
    }
    #[test]
    fn extract_specs_cache_survives_miss_then_hit() {
        // 미스 분기에서 읽기 가드를 잡은 채 쓰기 러을 잡으면 자기 교잭이므로, 같은 패턴을
        // 단일 스레드에서 다시 요구해 미스와 힌트 경로를 모두 통과시플.
        let request = json!({ "extract": [{ "name": "amount", "regex": "cache-probe-([0-9]+)" }] });
        let first = extract_specs(&request).unwrap();
        let second = extract_specs(&request).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].name, "amount");
        assert!(second[0].regex.is_match("cache-probe-1200"));
    }
}
