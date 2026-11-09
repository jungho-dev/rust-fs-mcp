//! replace.rs
//! tools::fs::replace
//!
//! file-replace handler: sd-style regex, literal, and ast-grep rewrites with all-or-nothing writes.
//! Phase 1 plans every item in memory (items on one file chain in order); phase 2 encodes, then writes each file once.
//! Keeps the file's legacy encoding, BOM, and CRLF line endings; a dryRun item turns the whole call into an encoded preview.
//! Fancy-regex match spans stream from a kill-able worker process (core::isolate) within the item's timeout_ms.
//! Each streamed span is validated and spliced on arrival, so the output cap and maxReplacements bound it like in-process.
//!

use super::write::write_atomic;
use crate::core::batch::{create_batch_response, run_batch};
use crate::core::config::{canonical_key, ensure_path_allowed};
use crate::core::diff::{DiffSpec, DiffSummary, render_unified_diff};
use crate::core::encoding::{decode_bytes, encode_text};
use crate::core::isolate::{
    FancyPattern, SpanMatch, WorkerError, build_fancy_regex, stream_fancy_spans,
};
use crate::core::response::{RawResult, max_output_bytes};
use crate::tools::common::{bool_field, item_content};
use crate::tools::search::{AstEdit, AstRewriteSpec, collect_ast_edits};
use encoding_rs::{Encoding, UTF_8};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
// 치환 결과 상한 (디코드 입력 최대 96 MiB)
const MAX_RESULT_BYTES: usize = 256 * 1024 * 1024;
const MAX_LISTED_LINES: usize = 10;
// 항목 시간 예산 (fs-search timeout_ms 기본값)
const DEFAULT_TIMEOUT_MS: u64 = 25_000;
// Instant 오버플로 방지 상한 (24시간)
const MAX_TIMEOUT_MS: u64 = 86_400_000;
const MIN_DIFF_BUDGET: usize = 2_048;
const UTF8_BOM: char = '\u{FEFF}';

// 1. Plan types ---------------------------------------------------------------
enum Matcher {
    Linear(regex::Regex),
    Fancy(fancy_regex::Regex),
}
enum Piece {
    Text(String),
    Group(usize),
}
struct MatchFlags {
    literal: bool,
    ignore_case: bool,
    dot_all: bool,
    word_match: bool,
}
// 항목별 timeout_ms 기한
struct ItemBudget {
    deadline: Instant,
    timeout_ms: u64,
}
// 호출 내 파일별 메모리 상태
struct FileState {
    path: PathBuf,
    display: String,
    encoding: &'static Encoding,
    bom: bool,
    bom_char: bool,
    current: String,
    changed: bool,
    len: u64,
    modified: Option<SystemTime>,
}
struct ItemPlan {
    file: usize,
    replacements: usize,
    engine: &'static str,
    lines: Vec<usize>,
    diff: Option<DiffSummary>,
    isolated: bool,
}
struct Replaced {
    text: String,
    count: usize,
    lines: Vec<usize>,
    // 격리 워커 스팬 사용
    isolated: bool,
}
enum CommitOutcome {
    Aborted { file: usize, message: String },
    Finished(Vec<Result<Option<usize>, String>>),
}

// 2. File replace tool --------------------------------------------------------
pub fn handle_file_replace(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    // 사전 계산 결과의 순서 보존 배치
    let mut results = replace_items(items).into_iter();
    let batch = run_batch(items, |_| {
        results
            .next()
            .unwrap_or_else(|| RawResult::error("file-replace lost an item result"))
    });
    create_batch_response("file-replace", batch, true)
}
fn replace_items(items: &[Value]) -> Vec<RawResult> {
    // 1. 전 항목 메모리 계획
    let mut files: Vec<FileState> = Vec::new();
    let mut keys: HashMap<String, usize> = HashMap::new();
    let plans: Vec<Result<ItemPlan, String>> = items
        .iter()
        .map(|item| plan_item(item, &mut files, &mut keys))
        .collect();

    // 2. 검증 실패 시 전체 미적용
    let failed: Vec<String> = plans
        .iter()
        .enumerate()
        .filter(|(_, plan)| plan.is_err())
        .map(|(index, _)| (index + 1).to_string())
        .collect();
    if !failed.is_empty() {
        let noun = if failed.len() == 1 { "item" } else { "items" };
        let note = format!(
            "not applied: {noun} {} failed validation; nothing was written",
            failed.join(", ")
        );
        return plans
            .into_iter()
            .map(|plan| match plan {
                Ok(_) => RawResult::error(note.clone()),
                Err(error) => RawResult::error(error),
            })
            .collect();
    }
    let plans: Vec<ItemPlan> = plans.into_iter().flatten().collect();
    let dry_run = items.iter().any(|item| bool_field(item, "dryRun", false));
    let budget = (max_output_bytes() * 4 / 5 / items.len().max(1)).max(MIN_DIFF_BUDGET);

    // 3. 전 파일 선인코딩 (dryRun 포함)
    let outcome = match encode_files(&files) {
        Err((file, message)) => CommitOutcome::Aborted { file, message },
        // 미리보기 (기록 예정 크기)
        Ok(encoded) if dry_run => {
            return plans
                .iter()
                .map(|plan| {
                    let bytes = encoded[plan.file].as_ref().map(|bytes| bytes.len());
                    render_item(plan, &files[plan.file], bytes, true, budget)
                })
                .collect();
        }
        Ok(encoded) => commit_files(&files, &encoded),
    };

    // 4. 커밋 결과
    match outcome {
        CommitOutcome::Aborted { file, message } => {
            let note = format!(
                "not applied: {} failed before any write; nothing was written",
                files[file].display
            );
            plans
                .iter()
                .map(|plan| {
                    if plan.file == file {
                        RawResult::error(message.clone())
                    } else {
                        RawResult::error(note.clone())
                    }
                })
                .collect()
        }
        CommitOutcome::Finished(outcomes) => plans
            .iter()
            .map(|plan| match &outcomes[plan.file] {
                Ok(bytes) => render_item(plan, &files[plan.file], *bytes, false, budget),
                Err(message) => RawResult::error(message.clone()),
            })
            .collect(),
    }
}

// 3. Item planning ------------------------------------------------------------
fn plan_item(
    item: &Value,
    files: &mut Vec<FileState>,
    keys: &mut HashMap<String, usize>,
) -> Result<ItemPlan, String> {
    // 1. 인자 검증
    let started = Instant::now();
    let Some(file_path) = item.get("file_path").and_then(Value::as_str) else {
        return Err("file_path must be a string".to_string());
    };
    let mode = item.get("mode").and_then(Value::as_str).unwrap_or("regex");
    if !matches!(mode, "regex" | "literal" | "ast") {
        return Err(format!("Unknown mode '{mode}'; use regex, literal, or ast"));
    }
    let pattern = item_content(item, "pattern", "pattern_path")?;
    if pattern.is_empty() {
        return Err("pattern must not be empty".to_string());
    }
    let replacement = item_content(item, "replacement", "replacement_path")?;
    let limit = match item.get("maxReplacements") {
        None | Some(Value::Null) => 0,
        Some(value) => value
            .as_u64()
            .ok_or("maxReplacements must be a non-negative integer")?
            as usize,
    };
    let expected = match item.get("expectedReplacements") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or("expectedReplacements must be a non-negative integer")? as usize,
        ),
    };
    let timeout_ms = match item.get("timeout_ms") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
        Some(value) => value
            .as_u64()
            .or_else(|| {
                value
                    .as_f64()
                    .filter(|ms| ms.is_finite() && *ms >= 0.0)
                    .map(|ms| ms as u64)
            })
            .ok_or("timeout_ms must be a non-negative number of milliseconds")?,
    };
    let budget = ItemBudget {
        deadline: started + Duration::from_millis(timeout_ms.min(MAX_TIMEOUT_MS)),
        timeout_ms,
    };
    let flags = MatchFlags {
        literal: mode == "literal",
        ignore_case: bool_field(item, "ignoreCase", false),
        dot_all: bool_field(item, "dotAll", false),
        word_match: bool_field(item, "wordMatch", false),
    };
    if mode == "ast" && (flags.ignore_case || flags.dot_all || flags.word_match) {
        return Err(
            "ignoreCase, dotAll, and wordMatch do not apply to mode \"ast\"; express them in the ast-grep pattern"
                .to_string(),
        );
    }

    // 2. 파일 상태 (같은 파일 연쇄 적용)
    let path = ensure_path_allowed(file_path)?;
    let key = canonical_key(&path.to_string_lossy());
    let index = match keys.get(&key) {
        Some(&index) => index,
        None => {
            files.push(load_file_state(path)?);
            keys.insert(key, files.len() - 1);
            files.len() - 1
        }
    };
    let state = &mut files[index];

    // 3. 엔진별 메모리 치환
    let (replaced, engine) = if mode == "ast" {
        if state.encoding != UTF_8 || state.bom_char {
            return Err(format!(
                "mode \"ast\" needs a UTF-8 file without BOM because ast-grep reports UTF-8 byte offsets; {} is {}",
                state.display,
                if state.bom_char {
                    "UTF-8 with BOM"
                } else {
                    state.encoding.name()
                }
            ));
        }
        // ast-grep 디스크 재판독
        if state.changed {
            return Err(format!(
                "mode \"ast\" reads {} from disk, so it cannot follow another edit of that file in the same call; run it in a separate call",
                state.display
            ));
        }
        let edits = collect_ast_edits(&AstRewriteSpec {
            path: &state.path,
            pattern: &pattern,
            rewrite: &replacement,
            lang: item.get("lang").and_then(Value::as_str),
            timeout_ms: u64::try_from(
                budget
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
            .max(1),
        })?;
        let replaced = apply_ast_edits(&state.current, &edits, limit)
            .map_err(|error| format!("{error} ({})", state.display))?;
        (replaced, "ast-grep")
    } else {
        let crlf = state.current.contains("\r\n");
        let (matcher, engine) = compile_matcher(&pattern, &flags, crlf)?;
        let template = if flags.literal {
            vec![Piece::Text(replacement)]
        } else {
            let names: Vec<Option<&str>> = match &matcher {
                Matcher::Linear(regex) => regex.capture_names().collect(),
                Matcher::Fancy(regex) => regex.capture_names().collect(),
            };
            parse_replacement(&replacement, &names)?
        };
        // fancy 스팬 격리 워커 계산 (미활성 시 프로세스 내)
        let isolated = match &matcher {
            Matcher::Fancy(regex) => {
                let run = FancyRun {
                    regex,
                    flags: &flags,
                    crlf,
                    template: &template,
                    limit,
                };
                replace_in_worker(&state.current, &run, &budget)
            }
            Matcher::Linear(_) => Ok(None),
        };
        let replaced = match isolated {
            Ok(Some(replaced)) => Ok(replaced),
            Ok(None) => replace_with_matcher(&state.current, &matcher, &template, limit, &budget),
            Err(error) => Err(error),
        }
        .map_err(|error| format!("{error} ({})", state.display))?;
        (replaced, engine)
    };

    // 4. 기대 개수 가드
    if let Some(expected) = expected
        && replaced.count != expected
    {
        let listed: Vec<String> = replaced.lines.iter().map(usize::to_string).collect();
        let lines = if listed.is_empty() {
            String::new()
        } else {
            let more = if listed.len() == MAX_LISTED_LINES {
                ", ..."
            } else {
                ""
            };
            format!(" (lines {}{more})", listed.join(", "))
        };
        return Err(format!(
            "Expected {expected} replacement(s) but found {}{lines} in {}; nothing was written",
            replaced.count, state.display
        ));
    }

    // 5. 항목 diff와 상태 갱신
    let diff = if replaced.count == 0 {
        None
    } else {
        Some(render_unified_diff(
            &state.current,
            &replaced.text,
            &state.display,
            &state.display,
            &DiffSpec::default(),
        ))
    };
    if replaced.count > 0 && replaced.text != state.current {
        state.current = replaced.text;
        state.changed = true;
    }
    Ok(ItemPlan {
        file: index,
        replacements: replaced.count,
        engine,
        lines: replaced.lines,
        diff,
        isolated: replaced.isolated,
    })
}
fn load_file_state(path: PathBuf) -> Result<FileState, String> {
    let Ok(metadata) = fs::metadata(&path) else {
        return Err(format!("Path does not exist: {}", path.display()));
    };
    if metadata.is_dir() {
        return Err(format!("Path is a directory: {}", path.display()));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} is {} bytes; file-replace edits files up to 64 MiB",
            path.display(),
            metadata.len()
        ));
    }
    let bytes =
        fs::read(&path).map_err(|error| format!("Failed to read {}: {error}", path.display()))?;

    // 레거시 인코딩 보존 디코드
    let decoded = decode_bytes(bytes);
    if decoded.lossy {
        return Err(format!(
            "{} is not valid UTF-8, UTF-16, or EUC-KR text; refusing to edit",
            path.display()
        ));
    }
    // UTF-8 BOM 문자 분리 (첫 줄 ^ 매치)
    let mut text = decoded.text;
    let bom_char = text.starts_with(UTF8_BOM);
    if bom_char {
        text.drain(..UTF8_BOM.len_utf8());
    }
    Ok(FileState {
        display: path.display().to_string(),
        path,
        encoding: decoded.encoding,
        bom: decoded.bom,
        bom_char,
        current: text,
        changed: false,
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

// 4. Pattern compilation ------------------------------------------------------
fn compile_matcher(
    pattern: &str,
    flags: &MatchFlags,
    crlf: bool,
) -> Result<(Matcher, &'static str), String> {
    let body = if flags.literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };

    // 1. 패턴 단독 검증 (래핑 전)
    let compiled = compile_source(&body, flags, crlf)?;
    if !flags.word_match {
        return Ok(compiled);
    }

    // 2. wordMatch 래핑 ((?x) 주석 종료 개행)
    match compile_source(&format!(r"\b(?:{body})\b"), flags, crlf) {
        Err(_) if body.contains('#') => compile_source(&format!("\\b(?:{body}\n)\\b"), flags, crlf),
        wrapped => wrapped,
    }
}
// 엔진 선택 컴파일
fn compile_source(
    source: &str,
    flags: &MatchFlags,
    crlf: bool,
) -> Result<(Matcher, &'static str), String> {
    let error = match regex::RegexBuilder::new(source)
        .multi_line(true)
        .case_insensitive(flags.ignore_case)
        .dot_matches_new_line(flags.dot_all)
        .crlf(crlf)
        .build()
    {
        Ok(regex) => {
            let engine = if flags.literal { "literal" } else { "regex" };
            return Ok((Matcher::Linear(regex), engine));
        }
        Err(error) => error,
    };
    if let regex::Error::CompiledTooBig(limit) = error {
        return Err(format!(
            "Regex compiles past the {limit}-byte size limit; simplify the pattern"
        ));
    }

    // 구문 오류 (리터럴 폴백 없음)
    let message = error.to_string();
    if flags.literal || !(message.contains("look-around") || message.contains("backreference")) {
        return Err(format!("Invalid regex pattern: {message}"));
    }
    // look-around·역참조 전용 fancy-regex (워커 동일 빌더)
    let regex = build_fancy_regex(&FancyPattern {
        source,
        ignore_case: flags.ignore_case,
        dot_all: flags.dot_all,
        crlf,
    })
    .map_err(|error| format!("Invalid regex pattern (fancy-regex): {error}"))?;
    // 룩어라운드 내 \K 거부
    if fancy_regex::Expr::parse_tree(source)
        .is_ok_and(|tree| has_keep_out_in_look_around(&tree.expr, false))
    {
        return Err(
            r"Invalid regex pattern: \K inside a look-around makes fancy-regex report overlapping or reversed match spans; move \K out of the look-around"
                .to_string(),
        );
    }
    Ok((Matcher::Fancy(regex), "fancy-regex"))
}
fn has_keep_out_in_look_around(expr: &fancy_regex::Expr, inside: bool) -> bool {
    use fancy_regex::Expr;
    match expr {
        Expr::KeepOut => inside,
        Expr::LookAround(child, _) => has_keep_out_in_look_around(child, true),
        Expr::Concat(children) | Expr::Alt(children) => children
            .iter()
            .any(|child| has_keep_out_in_look_around(child, inside)),
        Expr::Group(child) => has_keep_out_in_look_around(child, inside),
        Expr::AtomicGroup(child) | Expr::Repeat { child, .. } => {
            has_keep_out_in_look_around(child, inside)
        }
        Expr::Conditional {
            condition,
            true_branch,
            false_branch,
        } => [condition, true_branch, false_branch]
            .iter()
            .any(|child| has_keep_out_in_look_around(child, inside)),
        Expr::DefineGroup { definitions } => has_keep_out_in_look_around(definitions, inside),
        _ => false,
    }
}
// sd validate_replace + 미존재 그룹 거부
fn parse_replacement(replacement: &str, names: &[Option<&str>]) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = replacement;
    while let Some(dollar) = rest.find('$') {
        literal.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            literal.push('$');
            rest = tail;
            continue;
        }

        // 1. ${name} / $name 참조 판독
        let (name, tail, braced) = if let Some(inner) = after.strip_prefix('{') {
            let Some(close) = inner.find('}') else {
                literal.push('$');
                rest = after;
                continue;
            };
            (&inner[..close], &inner[close + 1..], true)
        } else {
            let len = after
                .bytes()
                .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                .count();
            if len == 0 {
                literal.push('$');
                rest = after;
                continue;
            }
            (&after[..len], &after[len..], false)
        };
        let reference = &rest[dollar..rest.len() - tail.len()];
        let digits = name.bytes().take_while(u8::is_ascii_digit).count();

        // 2. 모호한 번호 참조
        if !braced && digits > 0 && digits < name.len() {
            let (number, suffix) = name.split_at(digits);
            return Err(format!(
                "The capture reference {reference} in the replacement is ambiguous; write ${{{number}}}{suffix} for group {number} followed by \"{suffix}\""
            ));
        }

        // 3. 그룹 존재 확인
        let index = if digits > 0 && digits == name.len() {
            name.parse::<usize>()
                .ok()
                .filter(|number| *number < names.len())
        } else {
            names.iter().position(|candidate| *candidate == Some(name))
        };
        let Some(index) = index else {
            let named: Vec<&str> = names.iter().flatten().copied().collect();
            let known = if named.is_empty() {
                String::new()
            } else {
                format!(", named: {}", named.join(", "))
            };
            return Err(format!(
                "The replacement references {reference}, but the pattern has groups $0-${}{known}; write $$ for a literal dollar sign",
                names.len().saturating_sub(1)
            ));
        };
        if !literal.is_empty() {
            pieces.push(Piece::Text(std::mem::take(&mut literal)));
        }
        pieces.push(Piece::Group(index));
        rest = tail;
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        pieces.push(Piece::Text(literal));
    }
    Ok(pieces)
}

// 5. Match splicing -----------------------------------------------------------
// 순차 치환 누적기 (한도·줄 번호)
struct Splice<'t> {
    text: &'t str,
    out: String,
    last: usize,
    count: usize,
    limit: usize,
    lines: Vec<usize>,
    line: usize,
    line_pos: usize,
    max_out: usize,
}
impl<'t> Splice<'t> {
    fn new(text: &'t str, limit: usize) -> Self {
        Self {
            text,
            out: String::with_capacity(text.len()),
            last: 0,
            count: 0,
            limit,
            lines: Vec::new(),
            line: 1,
            line_pos: 0,
            max_out: MAX_RESULT_BYTES,
        }
    }
    // 한도 도달 시 Ok(false), 겹침·역행 스팬 거부
    fn push_match(
        &mut self,
        start: usize,
        end: usize,
        insert: impl FnOnce(&mut String),
    ) -> Result<bool, String> {
        let text = self.text;
        let (Some(gap), Some(_)) = (text.get(self.last..start), text.get(start..end)) else {
            return Err(format!(
                "match span {start}..{end} overlaps the previous match ending at {} or runs backwards (\\K inside a look-around reports such spans); nothing was written",
                self.last
            ));
        };
        if self.lines.len() < MAX_LISTED_LINES {
            let counted = text
                .as_bytes()
                .get(self.line_pos..start)
                .unwrap_or_default();
            self.line += memchr::memchr_iter(b'\n', counted).count();
            self.line_pos = start;
            if self.lines.last() != Some(&self.line) {
                self.lines.push(self.line);
            }
        }
        self.out.push_str(gap);
        insert(&mut self.out);
        if self.out.len() > self.max_out {
            return Err(format!(
                "the replaced text would pass the {}-byte file-replace limit; nothing was written",
                self.max_out
            ));
        }
        self.last = end;
        self.count += 1;
        Ok(self.limit == 0 || self.count < self.limit)
    }
    fn finish(mut self) -> Result<Replaced, String> {
        let rest = self.text.get(self.last..).unwrap_or_default();
        self.out.push_str(rest);
        if self.out.len() > self.max_out {
            return Err(format!(
                "the replaced text would pass the {}-byte file-replace limit; nothing was written",
                self.max_out
            ));
        }
        Ok(Replaced {
            text: self.out,
            count: self.count,
            lines: self.lines,
            isolated: false,
        })
    }
}
fn replace_with_matcher(
    text: &str,
    matcher: &Matcher,
    template: &[Piece],
    limit: usize,
    budget: &ItemBudget,
) -> Result<Replaced, String> {
    let mut splice = Splice::new(text, limit);
    let needs_groups = template
        .iter()
        .any(|piece| matches!(piece, Piece::Group(_)));
    match matcher {
        // 그룹 미참조 고속 경로
        Matcher::Linear(regex) if !needs_groups => {
            for found in regex.find_iter(text) {
                let more = splice.push_match(found.start(), found.end(), |out| {
                    expand_template(out, template, |_| None)
                })?;
                if !more {
                    break;
                }
            }
        }
        Matcher::Linear(regex) => {
            for captures in regex.captures_iter(text) {
                let Some(whole) = captures.get(0) else {
                    continue;
                };
                let more = splice.push_match(whole.start(), whole.end(), |out| {
                    expand_template(out, template, |group| {
                        captures.get(group).map(|found| found.as_str())
                    })
                })?;
                if !more {
                    break;
                }
            }
        }
        Matcher::Fancy(regex) => {
            for captures in regex.captures_iter(text) {
                let captures = captures.map_err(|error| {
                    format!("fancy-regex stopped: {error}; simplify the pattern")
                })?;
                // 매치 간 시간 예산
                if Instant::now() >= budget.deadline {
                    return Err(format!(
                        "fancy-regex ran past this item's {} ms timeout_ms budget after {} match(es); nothing was written, narrow the pattern or raise timeout_ms",
                        budget.timeout_ms, splice.count
                    ));
                }
                let Some(whole) = captures.get(0) else {
                    continue;
                };
                let more = splice.push_match(whole.start(), whole.end(), |out| {
                    expand_template(out, template, |group| {
                        captures.get(group).map(|found| found.as_str())
                    })
                })?;
                if !more {
                    break;
                }
            }
        }
    }
    splice.finish()
}
fn expand_template<'h>(
    out: &mut String,
    template: &[Piece],
    group: impl Fn(usize) -> Option<&'h str>,
) {
    for piece in template {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Group(index) => {
                if let Some(found) = group(*index) {
                    out.push_str(found);
                }
            }
        }
    }
}

// 6. Isolated fancy-regex -----------------------------------------------------
struct FancyRun<'r> {
    regex: &'r fancy_regex::Regex,
    flags: &'r MatchFlags,
    crlf: bool,
    template: &'r [Piece],
    limit: usize,
}
// 워커 스팬 도착 즉시 치환, 미활성 시 Ok(None)
fn replace_in_worker(
    text: &str,
    run: &FancyRun<'_>,
    budget: &ItemBudget,
) -> Result<Option<Replaced>, String> {
    let needs_groups = run
        .template
        .iter()
        .any(|piece| matches!(piece, Piece::Group(_)));
    let groups = if needs_groups {
        run.regex.captures_len().saturating_sub(1)
    } else {
        0
    };
    let job = json!({
        "op": "fancy-spans",
        "pattern": run.regex.as_str(),
        "ignoreCase": run.flags.ignore_case,
        "dotAll": run.flags.dot_all,
        "crlf": run.crlf,
        "limit": run.limit,
        "groups": needs_groups,
        "text": text
    });
    let remaining = budget.deadline.saturating_duration_since(Instant::now());
    let mut splice = Splice::new(text, run.limit);
    let streamed = stream_fancy_spans(&job, groups, remaining, |found| {
        splice_span(&mut splice, found, run.template)
    });
    match streamed {
        Ok(_) => {
            let mut replaced = splice.finish()?;
            replaced.isolated = true;
            Ok(Some(replaced))
        }
        Err(WorkerError::Unavailable) => Ok(None),
        Err(WorkerError::TimedOut(_)) => Err(format!(
            "fancy-regex ran past this item's {} ms timeout_ms budget, so its isolated worker was killed; nothing was written, narrow the pattern or raise timeout_ms",
            budget.timeout_ms
        )),
        Err(WorkerError::Job(message)) => Err(message),
        Err(error) => Err(format!("fancy-regex failed: {error}; nothing was written")),
    }
}
// 워커 스팬 1건 검증 후 치환 (겹침·역행·범위)
fn splice_span(
    splice: &mut Splice<'_>,
    found: &SpanMatch<'_>,
    template: &[Piece],
) -> Result<bool, String> {
    let text = splice.text;
    let outside = found
        .groups
        .iter()
        .flatten()
        .any(|(from, to)| text.get(*from..*to).is_none());
    if outside {
        return Err(
            "fancy-regex failed: the isolated worker sent unusable match spans (capture span outside the text); nothing was written"
                .to_string(),
        );
    }
    let (start, end) = (found.start, found.end);
    splice.push_match(start, end, |out| {
        expand_template(out, template, |group| match group {
            0 => text.get(start..end),
            _ => found
                .groups
                .get(group - 1)
                .copied()
                .flatten()
                .and_then(|(from, to)| text.get(from..to)),
        })
    })
}
// 외부 오프셋 검증 후 적용
fn apply_ast_edits(text: &str, edits: &[AstEdit], limit: usize) -> Result<Replaced, String> {
    let mut previous_end = 0usize;
    for edit in edits {
        let ordered = previous_end <= edit.start && edit.start <= edit.end;
        if !ordered || text.get(edit.start..edit.end) != Some(edit.original.as_str()) {
            return Err(format!(
                "ast-grep offsets do not match the file content (match at line {}); nothing was written",
                edit.line
            ));
        }
        previous_end = edit.end;
    }
    let mut splice = Splice::new(text, limit);
    for edit in edits {
        if !splice.push_match(edit.start, edit.end, |out| out.push_str(&edit.replacement))? {
            break;
        }
    }
    splice.finish()
}

// 7. Commit and response ------------------------------------------------------
type Encoded<'f> = Vec<Option<Cow<'f, [u8]>>>;
// 변경 파일 선인코딩 (실패 시 무기록)
fn encode_files(files: &[FileState]) -> Result<Encoded<'_>, (usize, String)> {
    let mut encoded: Encoded<'_> = Vec::with_capacity(files.len());
    for (index, state) in files.iter().enumerate() {
        if !state.changed {
            encoded.push(None);
            continue;
        }
        let bytes = if state.bom_char {
            let mut body = String::with_capacity(state.current.len() + UTF8_BOM.len_utf8());
            body.push(UTF8_BOM);
            body.push_str(&state.current);
            encode_text(&body, state.encoding, state.bom)
                .map(|bytes| Cow::Owned(bytes.into_owned()))
        } else {
            encode_text(&state.current, state.encoding, state.bom)
        };
        match bytes {
            Ok(bytes) => encoded.push(Some(bytes)),
            Err(error) => {
                return Err((
                    index,
                    format!(
                        "Failed to encode {}: {error}; nothing was written",
                        state.display
                    ),
                ));
            }
        }
    }
    Ok(encoded)
}
fn commit_files(files: &[FileState], encoded: &[Option<Cow<'_, [u8]>>]) -> CommitOutcome {
    // 1. 판독 후 디스크 변경 감지
    for (index, state) in files.iter().enumerate().filter(|(_, state)| state.changed) {
        let unchanged = fs::metadata(&state.path).is_ok_and(|metadata| {
            metadata.len() == state.len && metadata.modified().ok() == state.modified
        });
        if !unchanged {
            return CommitOutcome::Aborted {
                file: index,
                message: format!(
                    "{} changed on disk after it was read; nothing was written, rerun file-replace",
                    state.display
                ),
            };
        }
    }

    // 2. 파일별 1회 원자 기록
    let mut outcomes: Vec<Result<Option<usize>, String>> = vec![Ok(None); files.len()];
    let mut written: Vec<&str> = Vec::new();
    let mut failure: Option<&str> = None;
    for (index, bytes) in encoded.iter().enumerate() {
        let Some(bytes) = bytes else {
            continue;
        };
        let state = &files[index];
        if let Some(failed) = failure {
            outcomes[index] = Err(format!(
                "not written: stopped after the write failure on {failed}"
            ));
            continue;
        }
        match write_atomic(&state.path, bytes) {
            Ok(()) => {
                outcomes[index] = Ok(Some(bytes.len()));
                written.push(&state.display);
            }
            Err(error) => {
                let pending: Vec<&str> = encoded
                    .iter()
                    .enumerate()
                    .skip(index)
                    .filter(|(_, bytes)| bytes.is_some())
                    .map(|(pending, _)| files[pending].display.as_str())
                    .collect();
                let done = if written.is_empty() {
                    "none".to_string()
                } else {
                    written.join(", ")
                };
                outcomes[index] = Err(format!(
                    "Failed to write {}: {error}; already written: {done}; not written: {}",
                    state.display,
                    pending.join(", ")
                ));
                failure = Some(&state.display);
            }
        }
    }
    CommitOutcome::Finished(outcomes)
}
fn render_item(
    plan: &ItemPlan,
    state: &FileState,
    bytes: Option<usize>,
    dry_run: bool,
    budget: usize,
) -> RawResult {
    let mut structured = json!({
        "file_path": state.display,
        "replacements": plan.replacements,
        "engine": plan.engine,
        "lines": plan.lines,
        "insertions": 0,
        "deletions": 0
    });

    // 1. 요약 줄과 항목 diff
    let mut text = if plan.replacements == 0 {
        format!("No matches in {}", state.display)
    } else {
        let verb = if dry_run { "Would replace" } else { "Replaced" };
        format!(
            "{verb} {} match(es) in {}",
            plan.replacements, state.display
        )
    };
    if let Some(diff) = &plan.diff {
        structured["insertions"] = json!(diff.insertions);
        structured["deletions"] = json!(diff.deletions);
        if diff.identical && !diff.eol_changed {
            text.push_str(" (content unchanged)");
        }
        if diff.eol_changed {
            text.push_str(" (line endings changed; the diff folds CRLF to LF)");
            structured["eolChanged"] = json!(true);
        }
        let unified = diff.unified.trim_end_matches('\n');
        if !unified.is_empty() {
            text.push('\n');
        }
        // 응답 예산 분할 절단
        if unified.len() <= budget {
            text.push_str(unified);
        } else {
            let mut cut = budget;
            while !unified.is_char_boundary(cut) {
                cut -= 1;
            }
            let cut = unified[..cut].rfind('\n').unwrap_or(cut);
            text.push_str(&unified[..cut]);
            text.push_str(&format!(
                "\n... diff truncated ({} more bytes)",
                unified.len() - cut
            ));
            structured["diffTruncated"] = json!(true);
        }
    }

    // 2. 기록·인코딩 표기
    if let Some(bytes) = bytes {
        structured["bytes"] = json!(bytes);
    }
    if dry_run {
        structured["dryRun"] = json!(true);
    }
    if plan.isolated {
        structured["isolated"] = json!(true);
    }
    if state.encoding != UTF_8 {
        structured["encoding"] = json!(state.encoding.name());
    }
    RawResult::structured(text, structured)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::isolate::{PROTOCOL_VERSION, SpanStream, write_fancy_spans};
    use crate::tools::fs::make_temp_dir;

    fn run_items(items: Value) -> Vec<RawResult> {
        replace_items(items.as_array().unwrap())
    }
    fn plain_flags() -> MatchFlags {
        MatchFlags {
            literal: false,
            ignore_case: false,
            dot_all: false,
            word_match: false,
        }
    }
    fn budget_ms(timeout_ms: u64) -> ItemBudget {
        ItemBudget {
            deadline: Instant::now() + Duration::from_millis(timeout_ms),
            timeout_ms,
        }
    }
    fn write_fixture(dir: &std::path::Path, name: &str, bytes: &[u8]) -> String {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path.display().to_string()
    }
    // 워커 stdout 재현 (기록 + 상태 줄)
    fn worker_output(job: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        let reply = write_fancy_spans(job, &mut out).unwrap();
        let body = match reply {
            Ok(result) => json!({ "v": PROTOCOL_VERSION, "ok": true, "result": result }),
            Err(error) => json!({ "v": PROTOCOL_VERSION, "ok": false, "error": error }),
        };
        out.extend_from_slice(body.to_string().as_bytes());
        out.push(b'\n');
        out
    }
    // 청크 분할 스트림 치환 (replace_in_worker 동일 소비자)
    fn splice_streamed(
        mut splice: Splice<'_>,
        output: &[u8],
        template: &[Piece],
        groups: usize,
        chunk: usize,
    ) -> Result<Replaced, String> {
        let mut stream = SpanStream::new(groups, |found: &SpanMatch<'_>| {
            splice_span(&mut splice, found, template)
        });
        for piece in output.chunks(chunk) {
            stream.feed(piece).map_err(|error| error.to_string())?;
        }
        stream.finish().map_err(|error| error.to_string())?;
        let mut replaced = splice.finish()?;
        replaced.isolated = true;
        Ok(replaced)
    }

    #[test]
    fn ambiguous_numbered_reference_is_rejected() {
        let dir = make_temp_dir("rust-fs-mcp-replace-ambiguous");
        let file = write_fixture(&dir, "a.txt", b"ab ab\n");
        let results =
            run_items(json!([{ "file_path": file, "pattern": "(a)(b)", "replacement": "$2x" }]));
        assert!(results[0].is_error);
        let text = results[0].content[0]["text"].as_str().unwrap();
        assert!(text.contains("${2}x"), "{text}");
        assert_eq!(std::fs::read(&file).unwrap(), b"ab ab\n");
        let underscore =
            run_items(json!([{ "file_path": file, "pattern": "(a)", "replacement": "$1_" }]));
        assert!(underscore[0].is_error);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn braced_and_named_references_expand() {
        let dir = make_temp_dir("rust-fs-mcp-replace-braced");
        let file = write_fixture(&dir, "a.txt", b"ab ab\n");
        let results = run_items(
            json!([{ "file_path": file, "pattern": "(a)(?P<tail>b)", "replacement": "${2}x$1${tail}$0" }]),
        );
        assert!(!results[0].is_error, "{:?}", results[0]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "bxabab bxabab\n");
        let data = results[0].structured.as_ref().unwrap();
        assert_eq!(data["replacements"], 2);
        assert_eq!(data["engine"], "regex");
        assert_eq!(data["lines"], json!([1]));
        assert!(data["bytes"].is_u64());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn unknown_group_references_are_rejected() {
        let dir = make_temp_dir("rust-fs-mcp-replace-unknown");
        let file = write_fixture(&dir, "a.txt", b"abc\n");
        for replacement in ["${nope}", "$3", "$word", "${}"] {
            let results = run_items(
                json!([{ "file_path": file, "pattern": "(?P<one>a)(b)", "replacement": replacement }]),
            );
            assert!(results[0].is_error, "{replacement}");
            let text = results[0].content[0]["text"].as_str().unwrap();
            assert!(text.contains("$$"), "{text}");
        }
        assert_eq!(std::fs::read(&file).unwrap(), b"abc\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn double_dollar_and_backslashes_stay_literal() {
        let dir = make_temp_dir("rust-fs-mcp-replace-dollar");
        let file = write_fixture(&dir, "a.txt", b"cost\n");
        let results = run_items(
            json!([{ "file_path": file, "pattern": "cost", "replacement": "$$5 \\n $ ${" }]),
        );
        assert!(!results[0].is_error, "{:?}", results[0]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "$5 \\n $ ${\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn literal_mode_keeps_dollar_references_verbatim() {
        let dir = make_temp_dir("rust-fs-mcp-replace-literal");
        let file = write_fixture(&dir, "a.txt", b"a.b axb A.B\n");
        let results = run_items(
            json!([{ "file_path": file, "mode": "literal", "pattern": "a.b", "replacement": "$1", "ignoreCase": true }]),
        );
        assert!(!results[0].is_error, "{:?}", results[0]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "$1 axb $1\n");
        assert_eq!(results[0].structured.as_ref().unwrap()["engine"], "literal");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn ignore_case_is_opt_in() {
        let dir = make_temp_dir("rust-fs-mcp-replace-case");
        let file = write_fixture(&dir, "a.txt", b"Hello hello\n");
        let sensitive =
            run_items(json!([{ "file_path": file, "pattern": "hello", "replacement": "X" }]));
        assert_eq!(sensitive[0].structured.as_ref().unwrap()["replacements"], 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "Hello X\n");
        let insensitive = run_items(
            json!([{ "file_path": file, "pattern": "HELLO", "replacement": "Y", "ignoreCase": true }]),
        );
        assert_eq!(
            insensitive[0].structured.as_ref().unwrap()["replacements"],
            1
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "Y X\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn word_match_respects_hangul_boundaries() {
        let dir = make_temp_dir("rust-fs-mcp-replace-word");
        let file = write_fixture(&dir, "a.txt", "한글 한글자 (한글)\n".as_bytes());
        let results = run_items(
            json!([{ "file_path": file, "pattern": "한글", "replacement": "X", "wordMatch": true }]),
        );
        assert_eq!(results[0].structured.as_ref().unwrap()["replacements"], 2);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "X 한글자 (X)\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn dot_all_spans_lines_only_when_requested() {
        let dir = make_temp_dir("rust-fs-mcp-replace-dotall");
        let file = write_fixture(&dir, "a.txt", b"start\nmid\nend\n");
        let plain =
            run_items(json!([{ "file_path": file, "pattern": "start.*end", "replacement": "X" }]));
        assert!(!plain[0].is_error, "{:?}", plain[0]);
        assert_eq!(plain[0].structured.as_ref().unwrap()["replacements"], 0);
        assert!(
            plain[0].content[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("No matches")
        );
        let spanning = run_items(
            json!([{ "file_path": file, "pattern": "start.*end", "replacement": "X", "dotAll": true }]),
        );
        assert_eq!(spanning[0].structured.as_ref().unwrap()["replacements"], 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "X\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn crlf_anchors_keep_line_endings() {
        let dir = make_temp_dir("rust-fs-mcp-replace-crlf");
        let file = write_fixture(&dir, "a.txt", b"a  \r\nb \r\nc\r\n");
        let results = run_items(json!([
            { "file_path": file, "pattern": r"[ \t]+$", "replacement": "" },
            { "file_path": file, "pattern": "^c$", "replacement": "C" }
        ]));
        assert!(!results[0].is_error && !results[1].is_error, "{results:?}");
        assert_eq!(std::fs::read(&file).unwrap(), b"a\r\nb\r\nC\r\n");
        // 줄 중간 CRLF 보존 (EOF 개행 제외)
        let spaced = write_fixture(&dir, "spaced.txt", b"x \r\ny\r\n");
        run_items(json!([{ "file_path": spaced, "pattern": r"\s+$", "replacement": "" }]));
        assert_eq!(std::fs::read(&spaced).unwrap(), b"x\r\ny");
        assert_eq!(
            results[0].structured.as_ref().unwrap()["lines"],
            json!([1, 2])
        );
        assert!(
            results[0]
                .structured
                .as_ref()
                .unwrap()
                .get("eolChanged")
                .is_none()
        );
        let folded =
            run_items(json!([{ "file_path": file, "pattern": "\r\n", "replacement": "\n" }]));
        assert_eq!(folded[0].structured.as_ref().unwrap()["eolChanged"], true);
        assert_eq!(std::fs::read(&file).unwrap(), b"a\nb\nC\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn legacy_encodings_round_trip_byte_exact() {
        let dir = make_temp_dir("rust-fs-mcp-replace-legacy");
        let (euc, _, _) = encoding_rs::EUC_KR.encode("한글 값\n");
        let euc_file = write_fixture(&dir, "euc.txt", &euc);
        let mut wide = vec![0xFF, 0xFE];
        for unit in "가나 x\r\n".encode_utf16() {
            wide.extend_from_slice(&unit.to_le_bytes());
        }
        let wide_file = write_fixture(&dir, "wide.txt", &wide);
        let results = run_items(json!([
            { "file_path": euc_file, "pattern": "값", "replacement": "수정" },
            { "file_path": wide_file, "pattern": "x$", "replacement": "y" }
        ]));
        assert!(!results[0].is_error && !results[1].is_error, "{results:?}");
        let (expected, _, _) = encoding_rs::EUC_KR.encode("한글 수정\n");
        assert_eq!(std::fs::read(&euc_file).unwrap(), expected.as_ref());
        assert_eq!(
            results[0].structured.as_ref().unwrap()["encoding"],
            "EUC-KR"
        );
        let mut expected_wide = vec![0xFF, 0xFE];
        for unit in "가나 y\r\n".encode_utf16() {
            expected_wide.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(std::fs::read(&wide_file).unwrap(), expected_wide);
        assert_eq!(
            results[1].structured.as_ref().unwrap()["encoding"],
            "UTF-16LE"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn utf8_bom_is_kept_and_first_line_anchor_matches() {
        let dir = make_temp_dir("rust-fs-mcp-replace-bom");
        let file = write_fixture(&dir, "a.txt", b"\xEF\xBB\xBFfirst\nsecond\n");
        let results =
            run_items(json!([{ "file_path": file, "pattern": "^first", "replacement": "1st" }]));
        assert_eq!(results[0].structured.as_ref().unwrap()["replacements"], 1);
        assert_eq!(std::fs::read(&file).unwrap(), b"\xEF\xBB\xBF1st\nsecond\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn max_replacements_limits_in_file_order() {
        let dir = make_temp_dir("rust-fs-mcp-replace-max");
        let file = write_fixture(&dir, "a.txt", b"a a\na a\n");
        let results = run_items(
            json!([{ "file_path": file, "pattern": "a", "replacement": "b", "maxReplacements": 3 }]),
        );
        let data = results[0].structured.as_ref().unwrap();
        assert_eq!(data["replacements"], 3);
        assert_eq!(data["lines"], json!([1, 2]));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "b b\nb a\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn expected_replacements_mismatch_writes_nothing() {
        let dir = make_temp_dir("rust-fs-mcp-replace-expected");
        let first = write_fixture(&dir, "first.txt", b"one\n");
        let second = write_fixture(&dir, "second.txt", b"x x\n");
        let results = run_items(json!([
            { "file_path": first, "pattern": "one", "replacement": "ONE" },
            { "file_path": second, "pattern": "x", "replacement": "y", "expectedReplacements": 5 }
        ]));
        assert!(results[0].is_error && results[1].is_error);
        let note = results[0].content[0]["text"].as_str().unwrap();
        assert!(note.contains("not applied: item 2"), "{note}");
        let real = results[1].content[0]["text"].as_str().unwrap();
        assert!(
            real.contains("Expected 5 replacement(s) but found 2 (lines 1)"),
            "{real}"
        );
        assert_eq!(std::fs::read(&first).unwrap(), b"one\n");
        assert_eq!(std::fs::read(&second).unwrap(), b"x x\n");
        let zero = run_items(
            json!([{ "file_path": first, "pattern": "absent", "replacement": "", "expectedReplacements": 0 }]),
        );
        assert!(!zero[0].is_error, "{:?}", zero[0]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn failing_second_item_leaves_first_file_untouched() {
        let dir = make_temp_dir("rust-fs-mcp-replace-atomic");
        let first = write_fixture(&dir, "first.txt", b"keep\n");
        let second = write_fixture(&dir, "second.txt", b"keep\n");
        let results = run_items(json!([
            { "file_path": first, "pattern": "keep", "replacement": "changed" },
            { "file_path": second, "pattern": "a[", "replacement": "b" }
        ]));
        assert!(results[0].is_error && results[1].is_error);
        let invalid = results[1].content[0]["text"].as_str().unwrap();
        assert!(invalid.contains("Invalid regex pattern"), "{invalid}");
        assert_eq!(std::fs::read(&first).unwrap(), b"keep\n");
        assert_eq!(std::fs::read(&second).unwrap(), b"keep\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn same_file_items_apply_sequentially() {
        let dir = make_temp_dir("rust-fs-mcp-replace-chain");
        let file = write_fixture(&dir, "a.txt", b"foo\nbar\n");
        let results = run_items(json!([
            { "file_path": file, "pattern": "foo", "replacement": "bar" },
            { "file_path": file, "pattern": "bar", "replacement": "baz", "expectedReplacements": 2 }
        ]));
        assert!(!results[0].is_error && !results[1].is_error, "{results:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "baz\nbaz\n");
        assert_eq!(
            results[1].structured.as_ref().unwrap()["lines"],
            json!([1, 2])
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn dry_run_returns_diff_without_writing() {
        let dir = make_temp_dir("rust-fs-mcp-replace-dry");
        let file = write_fixture(&dir, "a.txt", b"foo\nkeep\n");
        let results = run_items(
            json!([{ "file_path": file, "pattern": "foo", "replacement": "bar", "dryRun": true }]),
        );
        assert!(!results[0].is_error, "{:?}", results[0]);
        let text = results[0].content[0]["text"].as_str().unwrap();
        assert!(text.starts_with("Would replace 1 match(es)"), "{text}");
        assert!(text.contains("-foo\n+bar"), "{text}");
        let data = results[0].structured.as_ref().unwrap();
        assert_eq!(data["dryRun"], true);
        assert_eq!(data["bytes"], 9);
        assert_eq!(
            (data["insertions"].as_u64(), data["deletions"].as_u64()),
            (Some(1), Some(1))
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"foo\nkeep\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn look_around_and_backreferences_switch_to_fancy() {
        let dir = make_temp_dir("rust-fs-mcp-replace-fancy");
        let file = write_fixture(&dir, "a.txt", b"foobar foobaz\naa bb cd\n");
        let results = run_items(json!([
            { "file_path": file, "pattern": "foo(?=bar)", "replacement": "X" },
            { "file_path": file, "pattern": r"(\w)\1", "replacement": "<$1>" }
        ]));
        assert!(!results[0].is_error && !results[1].is_error, "{results:?}");
        assert_eq!(
            results[0].structured.as_ref().unwrap()["engine"],
            "fancy-regex"
        );
        assert_eq!(results[1].structured.as_ref().unwrap()["replacements"], 3);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "Xbar f<o>baz\n<a> <b> cd\n"
        );
        let ambiguous =
            run_items(json!([{ "file_path": file, "pattern": r"(\w)\1", "replacement": "$1x" }]));
        assert!(ambiguous[0].is_error);
        let crlf = write_fixture(&dir, "crlf.txt", b"ab\r\ncb\r\n");
        let anchored =
            run_items(json!([{ "file_path": crlf, "pattern": "(?<=a)b$", "replacement": "X" }]));
        assert_eq!(
            anchored[0].structured.as_ref().unwrap()["engine"],
            "fancy-regex"
        );
        assert_eq!(std::fs::read(&crlf).unwrap(), b"aX\r\ncb\r\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn keep_out_inside_look_around_is_rejected() {
        let dir = make_temp_dir("rust-fs-mcp-replace-keep-out");
        for (content, pattern) in [
            ("yyy", r"(?<=\K.)y"),
            ("abcyz", r"(?<=\K...)z|y"),
            ("yyy", r"y(?=.\K)"),
        ] {
            let file = write_fixture(&dir, "a.txt", content.as_bytes());
            let results =
                run_items(json!([{ "file_path": file, "pattern": pattern, "replacement": "X" }]));
            assert!(results[0].is_error, "{pattern}: {:?}", results[0]);
            let text = results[0].content[0]["text"].as_str().unwrap();
            assert!(text.contains(r"\K"), "{pattern}: {text}");
            assert_eq!(
                std::fs::read(&file).unwrap(),
                content.as_bytes(),
                "{pattern}"
            );
        }
        // 룩어라운드 밖 \K 유지
        let file = write_fixture(&dir, "b.txt", b"foobar\n");
        let kept = run_items(
            json!([{ "file_path": file, "pattern": r"(?<=f)oo\Kbar", "replacement": "X" }]),
        );
        assert!(!kept[0].is_error, "{:?}", kept[0]);
        assert_eq!(std::fs::read(&file).unwrap(), b"fooX\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn dry_run_runs_the_encode_step() {
        let dir = make_temp_dir("rust-fs-mcp-replace-dry-encode");
        let (euc, _, _) = encoding_rs::EUC_KR.encode("한글 값\n");
        let file = write_fixture(&dir, "euc.txt", &euc);
        let preview = run_items(
            json!([{ "file_path": file, "pattern": "값", "replacement": "🙂", "dryRun": true }]),
        );
        assert!(preview[0].is_error, "{:?}", preview[0]);
        let text = preview[0].content[0]["text"].as_str().unwrap();
        assert!(text.contains("Failed to encode"), "{text}");
        let sized = run_items(
            json!([{ "file_path": file, "pattern": "값", "replacement": "수정", "dryRun": true }]),
        );
        assert!(!sized[0].is_error, "{:?}", sized[0]);
        let (expected, _, _) = encoding_rs::EUC_KR.encode("한글 수정\n");
        assert_eq!(
            sized[0].structured.as_ref().unwrap()["bytes"],
            json!(expected.len())
        );
        assert_eq!(std::fs::read(&file).unwrap(), euc.as_ref());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn word_match_validates_the_pattern_alone() {
        let dir = make_temp_dir("rust-fs-mcp-replace-word-alone");
        let file = write_fixture(&dir, "a.txt", b"a b ab\n");
        let unbalanced = run_items(
            json!([{ "file_path": file, "pattern": "a)|(b", "replacement": "X", "wordMatch": true }]),
        );
        assert!(unbalanced[0].is_error, "{:?}", unbalanced[0]);
        let text = unbalanced[0].content[0]["text"].as_str().unwrap();
        assert!(text.contains("Invalid regex pattern"), "{text}");
        assert_eq!(std::fs::read(&file).unwrap(), b"a b ab\n");
        let commented = write_fixture(&dir, "b.txt", b"foo food\n");
        let results = run_items(json!([
            { "file_path": commented, "pattern": "(?x)foo # note", "replacement": "X", "wordMatch": true },
            { "file_path": commented, "pattern": r"(?x)(?<=\s) food # note", "replacement": "Y", "wordMatch": true }
        ]));
        assert!(!results[0].is_error && !results[1].is_error, "{results:?}");
        assert_eq!(std::fs::read(&commented).unwrap(), b"X Y\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn splice_rejects_reversed_spans_and_oversized_output() {
        let mut splice = Splice::new("abcd", 0);
        assert_eq!(splice.push_match(2, 3, |out| out.push('X')), Ok(true));
        let overlap = splice.push_match(1, 4, |out| out.push('Y')).unwrap_err();
        assert!(overlap.contains(r"\K"), "{overlap}");
        assert!(splice.push_match(4, 3, |out| out.push('Y')).is_err());
        assert!(splice.push_match(3, 9, |out| out.push('Y')).is_err());
        assert_eq!(splice.finish().unwrap().text, "abXd");
        let mut grown = Splice::new("ab", 0);
        grown.max_out = 4;
        let oversized = grown.push_match(0, 1, |out| out.push_str("xxxxx"));
        assert!(oversized.unwrap_err().contains("4-byte"));
        let mut tail = Splice::new("abcdef", 0);
        tail.max_out = 4;
        assert_eq!(tail.push_match(0, 1, |out| out.push('x')), Ok(true));
        assert!(tail.finish().is_err());
    }
    #[test]
    fn fancy_matching_stops_at_the_deadline() {
        let (matcher, engine) = compile_matcher("a(?=b)", &plain_flags(), false).unwrap();
        assert_eq!(engine, "fancy-regex");
        let template = vec![Piece::Text("X".to_string())];
        let expired = replace_with_matcher("ab ab", &matcher, &template, 0, &budget_ms(0));
        let message = expired.err().unwrap();
        assert!(message.contains("0 ms timeout_ms budget"), "{message}");
        let replaced =
            replace_with_matcher("ab ab", &matcher, &template, 0, &budget_ms(60_000)).unwrap();
        assert_eq!(replaced.text, "Xb Xb");
        let dir = make_temp_dir("rust-fs-mcp-replace-deadline");
        let file = write_fixture(&dir, "a.txt", b"ab\n");
        let results = run_items(
            json!([{ "file_path": file, "pattern": "a(?=b)", "replacement": "X", "timeout_ms": 0 }]),
        );
        let text = results[0].content[0]["text"].as_str().unwrap();
        assert!(
            results[0].is_error && text.contains("timeout_ms budget"),
            "{text}"
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"ab\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn timeout_ms_is_validated_per_item() {
        let dir = make_temp_dir("rust-fs-mcp-replace-timeout");
        let file = write_fixture(&dir, "a.txt", b"ab\n");
        let negative = run_items(
            json!([{ "file_path": file, "pattern": "a", "replacement": "X", "timeout_ms": -1 }]),
        );
        let text = negative[0].content[0]["text"].as_str().unwrap();
        assert!(text.contains("timeout_ms must be"), "{text}");
        // 거대값·소수 허용 (Instant 오버플로 없음)
        let results = run_items(json!([
            { "file_path": file, "pattern": "a(?=b)", "replacement": "X", "timeout_ms": u64::MAX, "dryRun": true },
            { "file_path": file, "pattern": "b", "replacement": "Y", "timeout_ms": 1500.5 }
        ]));
        assert!(!results[0].is_error && !results[1].is_error, "{results:?}");
        assert_eq!(std::fs::read(&file).unwrap(), b"ab\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn worker_spans_splice_like_the_in_process_engine() {
        let many = "ab\n".repeat(200_000);
        let cases = [
            ("foobar foobaz\naa bb cd\n", "foo(?=bar)", "X", 0),
            ("foobar foobaz\naa bb cd\n", r"(\w)\1", "<$1>", 0),
            ("한글a 한글b 한글a\n", r"(?<g>한글)(?=a)", "${g}!$0", 0),
            ("x1 y2 x3 y4\n", r"(?:(x)|(y))(?=\d)", "[$1|$2]", 3),
            ("aaaa\n", r"(?=a)", "-", 0),
            ("foobar\n", r"(?<=f)oo\Kbar", "X", 0),
            ("foobar fox\n", r"(?<=(f))o+", "$1", 0),
            (many.as_str(), r"(a)(?=b)", "$1$1", 0),
            (many.as_str(), r"(a)(?=b)", "$1$1", 5),
        ];
        for (text, pattern, replacement, limit) in cases {
            let (matcher, _) = compile_matcher(pattern, &plain_flags(), false).unwrap();
            let Matcher::Fancy(regex) = &matcher else {
                panic!("{pattern} is not fancy");
            };
            let names: Vec<Option<&str>> = regex.capture_names().collect();
            let template = parse_replacement(replacement, &names).unwrap();
            let local =
                replace_with_matcher(text, &matcher, &template, limit, &budget_ms(60_000)).unwrap();
            let output = worker_output(&json!({
                "pattern": regex.as_str(),
                "text": text,
                "limit": limit,
                "groups": true
            }));
            let groups = regex.captures_len() - 1;
            let chunk = if text.len() > 1024 { 65_536 } else { 3 };
            let remote =
                splice_streamed(Splice::new(text, limit), &output, &template, groups, chunk)
                    .unwrap();
            assert_eq!(
                (remote.text.as_str(), remote.count, &remote.lines),
                (local.text.as_str(), local.count, &local.lines),
                "{pattern}"
            );
            assert!(remote.isolated && !local.isolated);
        }
    }
    #[test]
    fn streamed_spans_are_bounded_by_output_cap_not_span_volume() {
        // 스팬 기록 1.6 MB, 출력 변화 없음
        let text = "ab\n".repeat(200_000);
        let job = json!({ "pattern": r"(a)(?=b)", "text": text, "groups": true });
        let output = worker_output(&job);
        assert!(output.len() > 1_500_000, "{}", output.len());
        let template = vec![Piece::Group(1)];
        let same = splice_streamed(Splice::new(&text, 0), &output, &template, 1, 65_536).unwrap();
        assert_eq!((same.text.as_str(), same.count), (text.as_str(), 200_000));

        // 출력 상한 초과 (프로세스 내 동일 오류)
        let mut capped = Splice::new(&text, 0);
        capped.max_out = 64;
        let grown = vec![Piece::Group(1), Piece::Group(1)];
        let outcome = splice_streamed(capped, &output, &grown, 1, 65_536);
        assert_eq!(
            outcome.err().unwrap_or_default(),
            "the replaced text would pass the 64-byte file-replace limit; nothing was written"
        );
    }
    #[test]
    fn corrupt_worker_spans_are_rejected() {
        let template = vec![Piece::Group(1)];
        let done = |count: u64| {
            format!("{{\"v\":{PROTOCOL_VERSION},\"ok\":true,\"result\":{{\"count\":{count}}}}}\n")
        };
        for (output, detail) in [
            (
                format!("0,1,9,1\n{}", done(1)),
                "capture span outside the text",
            ),
            (
                format!("0,99,-\n{}", done(1)),
                "overlaps the previous match",
            ),
            (
                format!("1,2,-\n-1,1,-\n{}", done(2)),
                "overlaps the previous match",
            ),
            (format!("0,1,-\n{}", done(2)), "counted 2"),
            ("0,1,-\n".to_string(), "before its final status"),
            (
                format!(
                    "0,1,-\n{{\"v\":{PROTOCOL_VERSION},\"ok\":false,\"error\":\"fancy-regex stopped: x\"}}\n"
                ),
                "fancy-regex stopped: x",
            ),
        ] {
            let outcome =
                splice_streamed(Splice::new("abc", 0), output.as_bytes(), &template, 1, 4);
            let error = outcome.err().unwrap_or_default();
            assert!(error.contains(detail), "{output:?}: {error}");
        }
        let unchanged =
            splice_streamed(Splice::new("abc", 0), done(0).as_bytes(), &template, 1, 4).unwrap();
        assert_eq!((unchanged.text.as_str(), unchanged.count), ("abc", 0));
    }
    #[test]
    fn ast_mode_errors_propagate_without_writing() {
        let dir = make_temp_dir("rust-fs-mcp-replace-ast");
        let source = write_fixture(&dir, "a.rs", b"fn main() { foo(1); }\n");
        let other = write_fixture(&dir, "b.txt", b"keep\n");
        let results = run_items(json!([
            { "file_path": other, "pattern": "keep", "replacement": "lost" },
            { "file_path": source, "mode": "ast", "pattern": "foo($A)", "replacement": "bar($A)", "lang": "no-such-language" }
        ]));
        assert!(results[0].is_error && results[1].is_error, "{results:?}");
        assert_eq!(std::fs::read(&other).unwrap(), b"keep\n");
        assert_eq!(std::fs::read(&source).unwrap(), b"fn main() { foo(1); }\n");
        let (euc, _, _) = encoding_rs::EUC_KR.encode("한글\n");
        let legacy = write_fixture(&dir, "legacy.rs", &euc);
        let refused = run_items(
            json!([{ "file_path": legacy, "mode": "ast", "pattern": "x", "replacement": "y" }]),
        );
        let text = refused[0].content[0]["text"].as_str().unwrap();
        assert!(text.contains("UTF-8 file without BOM"), "{text}");
        let chained = run_items(json!([
            { "file_path": source, "pattern": "1", "replacement": "2" },
            { "file_path": source, "mode": "ast", "pattern": "foo($A)", "replacement": "bar($A)" }
        ]));
        let text = chained[1].content[0]["text"].as_str().unwrap();
        assert!(text.contains("separate call"), "{text}");
        assert_eq!(std::fs::read(&source).unwrap(), b"fn main() { foo(1); }\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn ast_edits_are_verified_against_the_text() {
        let edit = |start: usize, end: usize, original: &str| AstEdit {
            start,
            end,
            original: original.to_string(),
            replacement: "Z".to_string(),
            line: 1,
        };
        let text = "foo(1); foo(2);";
        let applied =
            apply_ast_edits(text, &[edit(0, 6, "foo(1)"), edit(8, 14, "foo(2)")], 0).unwrap();
        assert_eq!((applied.text.as_str(), applied.count), ("Z; Z;", 2));
        let limited =
            apply_ast_edits(text, &[edit(0, 6, "foo(1)"), edit(8, 14, "foo(2)")], 1).unwrap();
        assert_eq!(limited.text, "Z; foo(2);");
        assert!(apply_ast_edits(text, &[edit(0, 6, "bar(1)")], 0).is_err());
        assert!(apply_ast_edits(text, &[edit(8, 14, "foo(2)"), edit(0, 6, "foo(1)")], 0).is_err());
        assert!(apply_ast_edits(text, &[edit(10, 99, "x")], 0).is_err());
        assert!(apply_ast_edits("가", &[edit(1, 2, "")], 0).is_err());
    }
    #[test]
    fn invalid_arguments_are_rejected_before_reading() {
        let dir = make_temp_dir("rust-fs-mcp-replace-args");
        let file = write_fixture(&dir, "a.txt", b"x\n");
        for item in [
            json!({ "file_path": file, "pattern": "", "replacement": "y" }),
            json!({ "file_path": file, "pattern": "x" }),
            json!({ "file_path": file, "pattern": "x", "replacement": "y", "mode": "glob" }),
            json!({ "file_path": file, "pattern": "x", "replacement": "y", "maxReplacements": -1 }),
            json!({ "file_path": file, "mode": "ast", "pattern": "x", "replacement": "y", "ignoreCase": true }),
        ] {
            let results = run_items(json!([item]));
            assert!(results[0].is_error, "{item}");
        }
        assert_eq!(std::fs::read(&file).unwrap(), b"x\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn handler_wraps_results_in_one_batch() {
        let dir = make_temp_dir("rust-fs-mcp-replace-handler");
        let file = write_fixture(&dir, "a.txt", b"alpha\n");
        let response = handle_file_replace(&json!({
            "items": [{ "file_path": file, "pattern": "al(p)ha", "replacement": "${1}" }]
        }));
        assert!(!response.is_error, "{response:?}");
        let text = response.content[0]["text"].as_str().unwrap();
        assert!(
            text.contains("Replaced 1 match(es)") && text.contains("+p"),
            "{text}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "p\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn write_failure_reports_written_and_unwritten_files() {
        let dir = make_temp_dir("rust-fs-mcp-replace-readonly");
        let first = write_fixture(&dir, "first.txt", b"one\n");
        let locked = write_fixture(&dir, "locked.txt", b"one\n");
        let mut permissions = std::fs::metadata(&locked).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&locked, permissions.clone()).unwrap();
        let results = run_items(json!([
            { "file_path": first, "pattern": "one", "replacement": "two" },
            { "file_path": locked, "pattern": "one", "replacement": "two" }
        ]));
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&locked, permissions).unwrap();
        assert!(!results[0].is_error, "{:?}", results[0]);
        assert!(results[1].is_error);
        let text = results[1].content[0]["text"].as_str().unwrap();
        assert!(
            text.contains("already written: ") && text.contains("first.txt"),
            "{text}"
        );
        assert_eq!(std::fs::read(&first).unwrap(), b"two\n");
        assert_eq!(std::fs::read(&locked).unwrap(), b"one\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
