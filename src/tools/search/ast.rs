//! ast.rs
//! tools::search::ast
//!
//! ast-search handler over the ast-grep CLI (--json=stream output) plus the rewrite edits file-replace mode "ast" applies.
//! Matches are sorted by file then byte offset; rewrites render as unified diffs and never write.
//! Byte offsets are raw UTF-8 file offsets (BOM and CRLF included); lines and char columns are reported 1-based.
//! The search root is the ast-grep cwd, so the project's sgconfig.yml (languageGlobs, customLanguages) applies.
//! Matches are parsed while ast-grep streams; a timeout keeps the matches so far (timedOut), but rewrite edits stay all-or-nothing.
//!

use crate::core::batch::{SEARCH_PLAN, create_batch_response, run_batch_parallel};
use crate::core::config::ensure_path_allowed;
use crate::core::diff::{DiffSpec, MAX_DIFF_CONTEXT, render_unified_diff};
use crate::core::external::{ExternalTool, MAX_PIPE_BYTES, ast_grep_version, run_external_lines};
use crate::core::response::{RawResult, max_output_bytes};
use crate::tools::common::{bool_field, path_in_heavy_dir};
use serde::Deserialize;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

// 기본 타임아웃 (fs-search 동일)
const AST_TIMEOUT_MS: u64 = 25_000;
// 매치·메타변수·문맥 줄 표시 상한 (문자)
const CLIP_CHARS: usize = 300;
// rewrite diff 기본 문맥 줄
const DIFF_CONTEXT: usize = 3;

// 1. Rewrite contract ---------------------------------------------------------
// start/end: 파일 UTF-8 바이트 오프셋, line: 1부터
#[derive(Clone, Debug)]
pub(crate) struct AstEdit {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) original: String,
    pub(crate) replacement: String,
    pub(crate) line: usize,
}
pub(crate) struct AstRewriteSpec<'a> {
    pub(crate) path: &'a Path,
    pub(crate) pattern: &'a str,
    pub(crate) rewrite: &'a str,
    pub(crate) lang: Option<&'a str>,
    pub(crate) timeout_ms: u64,
}
// 단일 파일 rewrite 편집 수집
pub(crate) fn collect_ast_edits(spec: &AstRewriteSpec<'_>) -> Result<Vec<AstEdit>, String> {
    let version = ast_grep_version()?;
    let mut args = vec![
        "run".to_string(),
        format!("--pattern={}", spec.pattern),
        format!("--rewrite={}", spec.rewrite),
    ];
    if let Some(lang) = spec.lang {
        args.push(format!("--lang={lang}"));
    }
    args.extend(
        ["--color=never", "--json=stream", "--inspect=entity"]
            .iter()
            .map(|flag| flag.to_string()),
    );
    args.push(spec.path.to_string_lossy().into_owned());
    let run = run_ast_grep(
        &args,
        spec.path.parent(),
        spec.timeout_ms,
        version,
        spec.pattern,
    )?;
    // 부분 편집 집합 거절 (전부 아니면 무적용)
    if run.timed_out {
        return Err(format!(
            "{} timed out after {}ms on {}, so the rewrite set is incomplete; nothing was applied. Narrow the pattern or add lang, or raise timeout_ms",
            ExternalTool::AstGrep.backend_name(),
            spec.timeout_ms,
            spec.path.display()
        ));
    }
    if run.capped {
        return Err(format!(
            "ast-grep output for {} exceeded the {} MiB pipe cap, so the rewrite set is incomplete; nothing was applied. Narrow the pattern or split the file",
            spec.path.display(),
            MAX_PIPE_BYTES / (1024 * 1024)
        ));
    }
    if let Some(reason) = unparsed_file_reason(&run, spec.path) {
        return Err(reason);
    }
    let mut matches = run.matches;
    matches.sort_by_key(|found| (found.start, Reverse(found.end)));
    // 중첩 매치는 다음 패스 몫 (부분 겹침은 오류)
    let (outermost, _) = keep_outermost(&matches)?;
    let mut edits = Vec::with_capacity(outermost.len());
    for found in outermost {
        let Some(replacement) = &found.replacement else {
            return Err(format!(
                "ast-grep {version} --json=stream output lacks the field /replacement for a rewrite match"
            ));
        };
        edits.push(AstEdit {
            start: found.start,
            end: found.end,
            original: found.text.clone(),
            replacement: replacement.clone(),
            line: found.line + 1,
        });
    }
    Ok(edits)
}

// 2. AST search tool ----------------------------------------------------------
pub fn handle_ast_search(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };
    let results = run_batch_parallel(items, SEARCH_PLAN, |item| {
        run_ast_search(item).unwrap_or_else(RawResult::error)
    });
    create_batch_response("ast-search", results, true)
}

fn run_ast_search(item: &Value) -> Result<RawResult, String> {
    // 1. 인자 검증
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return Err("path must be a string".to_string());
    };
    let root = ensure_path_allowed(path)?;
    let Ok(metadata) = std::fs::metadata(&root) else {
        return Err(format!("Path does not exist: {}", root.display()));
    };
    let Some(pattern) = item.get("pattern").and_then(Value::as_str) else {
        return Err(
            "pattern must be a string (an ast-grep pattern such as $A.unwrap())".to_string(),
        );
    };
    let rewrite = item.get("rewrite").and_then(Value::as_str);
    let include_hidden = bool_field(item, "includeHidden", false);
    let context = item
        .get("contextLines")
        .and_then(Value::as_u64)
        .map(|lines| lines.min(MAX_DIFF_CONTEXT as u64) as usize);
    let max_results = item
        .get("maxResults")
        .and_then(Value::as_u64)
        .map(|limit| limit as usize)
        .unwrap_or(usize::MAX);
    let timeout_ms = item
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(AST_TIMEOUT_MS);
    let version = ast_grep_version()?;

    // 2. ast-grep 인자 구성 (- 시작 값 보호)
    let mut args = vec!["run".to_string(), format!("--pattern={pattern}")];
    for (flag, key) in [
        ("--lang", "lang"),
        ("--rewrite", "rewrite"),
        ("--strictness", "strictness"),
    ] {
        if let Some(text) = item.get(key).and_then(Value::as_str) {
            args.push(format!("{flag}={text}"));
        }
    }
    // 기본 제외 먼저 (뒤의 사용자 glob 우선)
    if !path_in_heavy_dir(&root) {
        args.push("--globs=!**/node_modules/**".to_string());
        args.push("--globs=!**/target/**".to_string());
        if include_hidden {
            args.push("--globs=!**/.git/**".to_string());
        }
    }
    match item.get("globs") {
        Some(Value::Array(globs)) => {
            for glob in globs {
                let Some(glob) = glob.as_str() else {
                    return Err("globs must be an array of strings".to_string());
                };
                args.push(format!("--globs={glob}"));
            }
        }
        Some(Value::String(glob)) => args.push(format!("--globs={glob}")),
        Some(Value::Null) | None => {}
        Some(_) => return Err("globs must be an array of strings".to_string()),
    }
    if include_hidden {
        args.push("--no-ignore=hidden".to_string());
    }
    // fs-search 동일 무시 규칙
    let ignore_kinds: &[&str] = if bool_field(item, "respectGitignore", true) {
        &["dot", "exclude", "global"]
    } else {
        &["dot", "vcs"]
    };
    args.extend(
        ignore_kinds
            .iter()
            .map(|kind| format!("--no-ignore={kind}")),
    );
    let inspect = if metadata.is_file() {
        "entity"
    } else {
        "summary"
    };
    args.push("--color=never".to_string());
    args.push("--json=stream".to_string());
    args.push(format!("--inspect={inspect}"));
    args.push(root.to_string_lossy().into_owned());

    // 3. 실행, 단일 파일 미파싱 거절
    let cwd = if metadata.is_dir() {
        Some(root.as_path())
    } else {
        root.parent()
    };
    let mut run = run_ast_grep(&args, cwd, timeout_ms, version, pattern)?;
    // 수집분 없는 타임아웃 오류
    if run.timed_out && run.matches.is_empty() {
        return Err(format!(
            "{} timed out after {timeout_ms}ms with no matches yet; narrow the path, add globs or lang, or raise timeout_ms",
            ExternalTool::AstGrep.backend_name()
        ));
    }
    if metadata.is_file()
        && let Some(reason) = unparsed_file_reason(&run, &root)
    {
        return Err(reason);
    }
    // 파일·시작 오프셋순, 같은 시작은 바깥 먼저
    run.matches.sort_by(|left, right| {
        (&left.file, left.start, Reverse(left.end)).cmp(&(
            &right.file,
            right.start,
            Reverse(right.end),
        ))
    });

    // 4. 본문 렌더링 (응답 예산 80%)
    let budget = max_output_bytes() * 4 / 5;
    let rendered = match rewrite {
        Some(_) => render_rewrite(&run.matches, context, max_results, budget, version)?,
        None => render_matches(
            &run.matches,
            context.unwrap_or(0),
            max_results,
            budget,
            metadata.is_dir(),
        ),
    };
    let limit_reached = rendered.limit_reached || run.capped;
    let mut notes = Vec::new();
    if run.timed_out {
        notes.push(
            "[search stopped at the time budget; results are partial. Narrow the path, add globs or lang, or raise timeout_ms]"
                .to_string(),
        );
    }
    if limit_reached {
        let total = if run.capped || run.timed_out {
            format!("{}+", run.matches.len())
        } else {
            run.matches.len().to_string()
        };
        notes.push(format!(
            "[stopped at maxResults or the output budget: {} of {total} matches shown; narrow path, globs, or lang]",
            rendered.match_count
        ));
    }
    if rendered.nested_skipped > 0 {
        notes.push(format!(
            "[{} nested matches inside a rewritten match are left for a second pass, as ast-grep -U does]",
            rendered.nested_skipped
        ));
    }
    if metadata.is_dir() && run.skipped > 0 {
        notes.push(format!(
            "[{} files skipped by ast-grep: not valid UTF-8 (EUC-KR, UTF-16), too large, or unreadable]",
            run.skipped
        ));
    }
    notes.extend(
        run.diagnostics
            .iter()
            .map(|line| format!("[ast-grep: {line}]")),
    );
    let mut text = notes.join("\n");
    if !text.is_empty() && !rendered.body.is_empty() {
        text.push('\n');
    }
    text.push_str(rendered.body.trim_end_matches('\n'));

    // 5. 구조화 메타
    let backend = ExternalTool::AstGrep.backend_name();
    let mut structured = json!({
        "backend": backend,
        "astGrepVersion": version,
        "totalCount": rendered.match_count,
        "fileCount": rendered.file_count,
    });
    if let Some(scanned) = run.scanned {
        structured["scannedFileCount"] = json!(scanned);
    }
    if run.skipped > 0 {
        structured["skippedFileCount"] = json!(run.skipped);
    }
    if limit_reached {
        structured["limitReached"] = json!(true);
    }
    if run.timed_out {
        structured["timedOut"] = json!(true);
    }
    if rewrite.is_some() {
        structured["rewrite"] = json!(true);
    }
    if rendered.nested_skipped > 0 {
        structured["nestedSkipped"] = json!(rendered.nested_skipped);
    }
    let mut result = RawResult::structured(text, structured);
    result.meta.insert("backend".to_string(), json!(backend));
    Ok(result)
}

// 3. ast-grep runner ----------------------------------------------------------
struct AstMatch {
    file: String,
    start: usize,
    end: usize,
    // 0부터 (ast-grep 원본)
    line: usize,
    end_line: usize,
    column: usize,
    text: String,
    replacement: Option<String>,
    metavars: Vec<Capture>,
}
// end: 매치 원문 기준 바이트 끝
struct Capture {
    name: String,
    text: String,
    end: Option<usize>,
}
struct AstRun {
    matches: Vec<AstMatch>,
    // 파이프 상한 도달 (부분 결과)
    capped: bool,
    // 기한 도달 (부분 결과)
    timed_out: bool,
    scanned: Option<usize>,
    skipped: usize,
    // --inspect=entity 파일별 판정
    parsed_files: usize,
    skip_reason: Option<String>,
    diagnostics: Vec<String>,
}
fn run_ast_grep(
    args: &[String],
    cwd: Option<&Path>,
    timeout_ms: u64,
    version: &str,
    pattern: &str,
) -> Result<AstRun, String> {
    // 1. 스트리밍 실행, 줄마다 파싱 (첫 오류 보관)
    let mut matches = Vec::new();
    let mut malformed: Option<String> = None;
    let output = run_external_lines(
        ExternalTool::AstGrep,
        args,
        cwd,
        Some(timeout_ms),
        &mut |line| {
            if malformed.is_some() || line.trim().is_empty() {
                return;
            }
            match parse_match(line, version) {
                Ok(found) => matches.push(found),
                Err(error) => malformed = Some(error),
            }
        },
    )?;

    // 2. stderr 분류
    let mut run = AstRun {
        matches,
        capped: output.stdout_capped,
        timed_out: output.timed_out,
        scanned: None,
        skipped: 0,
        parsed_files: 0,
        skip_reason: None,
        diagnostics: Vec::new(),
    };
    let stderr = strip_ansi(&output.stderr);
    // 메타변수 치환 문자 복원
    let stderr = if pattern.contains('µ') {
        stderr
    } else {
        stderr.replace('µ', "$")
    };
    for line in stderr.lines().map(str::trim) {
        if let Some(trace) = line.strip_prefix("sg: ") {
            if let Some(counts) = trace.strip_prefix("summary|file: ") {
                for pair in counts.split(',') {
                    let (key, count) = pair.split_once('=').unwrap_or((pair, ""));
                    let count = count.trim().parse::<usize>().ok();
                    match key.trim() {
                        "scannedFileCount" => run.scanned = count,
                        "skippedFileCount" => run.skipped = count.unwrap_or(0),
                        _ => {}
                    }
                }
            } else if trace.starts_with("entity|file|") {
                if let Some((_, reason)) = trace.rsplit_once(": skipped=") {
                    run.skip_reason.get_or_insert_with(|| reason.to_string());
                } else if trace.contains(": language=") {
                    run.parsed_files += 1;
                }
            }
            continue;
        }
        if line.is_empty()
            || line.starts_with("See also:")
            || line.starts_with("For more information")
        {
            continue;
        }
        run.diagnostics.push(line.to_string());
    }

    // 3. 종료 코드 (0/1 정상, 2·8 오류, kill 제외)
    let failure = match output.status_code {
        _ if output.stdout_capped || output.timed_out => None,
        Some(0) | Some(1) => None,
        Some(code) => Some(format!("exit {code}")),
        None => Some("terminated without an exit code".to_string()),
    };
    if let Some(failure) = failure {
        let detail = if run.diagnostics.is_empty() {
            "no stderr".to_string()
        } else {
            run.diagnostics.join("\n")
        };
        return Err(format!("ast-grep {version} failed ({failure}): {detail}"));
    }
    if let Some(error) = malformed {
        return Err(error);
    }

    // 4. 무매치 진단 오류화
    if run.matches.is_empty() && !run.diagnostics.is_empty() {
        let joined = run.diagnostics.join("\n");
        if joined.contains("ERROR node") {
            return Err(format!(
                "pattern did not parse cleanly and matched nothing; it must be one valid code node with $VAR / $$$ARGS placeholders (set lang when the file type is ambiguous). ast-grep {version}: {joined}"
            ));
        }
        if run
            .diagnostics
            .iter()
            .any(|line| line.starts_with("ERROR") || line.starts_with("Error"))
        {
            return Err(format!("ast-grep {version}: {joined}"));
        }
    }
    Ok(run)
}
// 단일 파일 미파싱 사유
fn unparsed_file_reason(run: &AstRun, path: &Path) -> Option<String> {
    run.scanned?;
    if let Some(reason) = &run.skip_reason {
        let detail = match reason.as_str() {
            "invalidUtf8" => {
                "it is not valid UTF-8 (ast-grep parses UTF-8 only; EUC-KR and UTF-16 files are skipped)"
            }
            "fileTooLarge" => "it is too large for ast-grep",
            other => other,
        };
        return Some(format!("ast-grep skipped {}: {detail}", path.display()));
    }
    if run.parsed_files == 0 && run.matches.is_empty() {
        return Some(format!(
            "ast-grep did not parse {}: its extension maps to no language (or not to lang); set lang to the file's language, or map the extension with languageGlobs in sgconfig.yml",
            path.display()
        ));
    }
    None
}
// --json=stream 레코드 (사용 필드만, 나머지 건너뜀)
#[derive(Deserialize)]
struct StreamRecord<'a> {
    #[serde(borrow)]
    file: Cow<'a, str>,
    #[serde(borrow)]
    text: Cow<'a, str>,
    range: RecordRange,
    #[serde(default, borrow)]
    replacement: Option<Cow<'a, str>>,
    #[serde(default, rename = "replacementOffsets")]
    replacement_offsets: Option<ByteSpan>,
    #[serde(default, rename = "metaVariables", borrow)]
    meta_variables: Option<MetaVariables<'a>>,
    // 첫 줄 BOM 판정용
    #[serde(default, borrow)]
    lines: Option<Cow<'a, str>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordRange {
    byte_offset: ByteSpan,
    start: StartPosition,
    end: EndPosition,
}
#[derive(Clone, Copy, Deserialize)]
struct ByteSpan {
    start: usize,
    end: usize,
}
#[derive(Deserialize)]
struct StartPosition {
    line: usize,
    column: usize,
}
#[derive(Deserialize)]
struct EndPosition {
    line: usize,
}
#[derive(Default, Deserialize)]
struct MetaVariables<'a> {
    #[serde(default, borrow)]
    single: BTreeMap<Cow<'a, str>, MetaNode<'a>>,
    #[serde(default, borrow)]
    multi: BTreeMap<Cow<'a, str>, Vec<MetaNode<'a>>>,
}
#[derive(Deserialize)]
struct MetaNode<'a> {
    #[serde(default, borrow)]
    text: Cow<'a, str>,
    #[serde(default)]
    range: Option<NodeRange>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeRange {
    #[serde(default)]
    byte_offset: Option<ByteSpan>,
}
// --json=stream 한 줄 파싱
fn parse_match(line: &str, version: &str) -> Result<AstMatch, String> {
    let record: StreamRecord<'_> = serde_json::from_str(line).map_err(|error| {
        format!(
            "ast-grep {version} printed malformed --json=stream output ({error}); validated with ast-grep 0.51.1"
        )
    })?;
    let ByteSpan { start, end } = record.range.byte_offset;
    let text = record.text.into_owned();
    let replacement = record.replacement.map(Cow::into_owned);
    // 확장 범위 rewrite 거절 (원문 검증 불가)
    if replacement.is_some()
        && let Some(replaced) = record.replacement_offsets
        && (replaced.start, replaced.end) != (start, end)
    {
        return Err(format!(
            "ast-grep {version} rewrite range {}..{} differs from the match range {start}..{end}; refusing an unverifiable edit",
            replaced.start, replaced.end
        ));
    }
    // 메타변수 원문 (multi는 바이트 구간)
    let meta = record.meta_variables.unwrap_or_default();
    let span_of = |node: Option<&MetaNode<'_>>| {
        node.and_then(|node| node.range.as_ref())
            .and_then(|range| range.byte_offset)
    };
    let mut metavars = Vec::with_capacity(meta.single.len() + meta.multi.len());
    for (name, capture) in &meta.single {
        metavars.push(Capture {
            name: format!("${name}"),
            text: capture.text.to_string(),
            end: span_of(Some(capture)).and_then(|span| span.end.checked_sub(start)),
        });
    }
    for (name, nodes) in &meta.multi {
        let span = span_of(nodes.first())
            .zip(span_of(nodes.last()))
            .and_then(|(first, last)| {
                let relative = first.start.checked_sub(start)?..last.end.checked_sub(start)?;
                Some((text.get(relative.clone())?, relative.end))
            });
        let (captured, end) = match span {
            Some((span, end)) => (span.to_string(), Some(end)),
            None => (
                nodes
                    .iter()
                    .map(|node| node.text.as_ref())
                    .collect::<Vec<_>>()
                    .join(" "),
                None,
            ),
        };
        metavars.push(Capture {
            name: format!("$$${name}"),
            text: captured,
            end,
        });
    }
    metavars.sort_by(|left, right| left.name.cmp(&right.name));
    // 첫 줄 BOM 문자 열 보정
    let line = record.range.start.line;
    let mut column = record.range.start.column;
    if line == 0
        && record
            .lines
            .as_deref()
            .is_some_and(|lines| lines.starts_with('\u{feff}'))
    {
        column = column.saturating_sub(1);
    }
    // 역전·overflow 좌표 거절
    let end_line = record.range.end.line;
    if start > end || end_line < line || end_line == usize::MAX || column == usize::MAX {
        return Err(format!(
            "ast-grep {version} --json=stream output has an invalid range (bytes {start}..{end}, lines {line}..{end_line}); validated with ast-grep 0.51.1"
        ));
    }
    Ok(AstMatch {
        file: record.file.into_owned(),
        start,
        end,
        line,
        end_line,
        column,
        text,
        replacement,
        metavars,
    })
}
// ANSI 색 제어열 제거
fn strip_ansi(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            plain.push(ch);
            continue;
        }
        if chars.next() == Some('[') {
            for code in chars.by_ref() {
                if ('@'..='~').contains(&code) {
                    break;
                }
            }
        }
    }
    plain
}

// 4. Output rendering ---------------------------------------------------------
struct Rendered {
    body: String,
    match_count: usize,
    file_count: usize,
    limit_reached: bool,
    nested_skipped: usize,
}
// 매치 목록 (fs-search --heading 형태)
fn render_matches(
    matches: &[AstMatch],
    context: usize,
    max_results: usize,
    budget: usize,
    heading: bool,
) -> Rendered {
    let mut rendered = Rendered {
        body: String::new(),
        match_count: 0,
        file_count: 0,
        limit_reached: false,
        nested_skipped: 0,
    };
    for group in matches.chunk_by(|left, right| left.file == right.file) {
        let Some(head) = group.first() else {
            continue;
        };
        // 문맥 줄 원본
        let source = if context > 0 {
            std::fs::read(&head.file)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let source_lines: Vec<&str> = source.split('\n').collect();
        // 단일 파일 루트는 배치 요약 줄이 경로
        let mut block = if heading {
            format!("{}\n", head.file)
        } else {
            String::new()
        };
        let mut kept = 0usize;
        let mut printed = 0usize;
        for (position, found) in group.iter().enumerate() {
            if rendered.match_count + kept >= max_results {
                rendered.limit_reached = true;
                break;
            }
            let line_no = found.line + 1;
            let end_no = found.end_line + 1;
            let mut entry = String::new();
            let push_context = |entry: &mut String, number: usize| {
                if let Some(source_line) = source_lines.get(number - 1) {
                    let _ = writeln!(entry, "{number}-{}", clip_display(source_line));
                }
            };
            if context > 0 {
                for number in line_no.saturating_sub(context).max(printed + 1)..line_no {
                    push_context(&mut entry, number);
                }
            }
            let shown = clip_display(&found.text);
            let _ = writeln!(entry, "{line_no}:{}: {shown}", found.column + 1);
            // 매치 줄에 이미 보이는 메타변수 생략
            let visible = shown.trim_end_matches('…').len();
            for capture in &found.metavars {
                if capture.text.is_empty() || capture.end.is_some_and(|end| end <= visible) {
                    continue;
                }
                let _ = writeln!(
                    entry,
                    "  {} = {}",
                    capture.name,
                    clip_display(&capture.text)
                );
            }
            printed = printed.max(end_no);
            if context > 0 {
                // 다음 매치 직전까지 (파일 끝 상한)
                let next_shown = rendered.match_count + kept + 1 < max_results;
                let next_line = group
                    .get(position + 1)
                    .filter(|_| next_shown)
                    .map(|next| next.line + 1)
                    .unwrap_or(usize::MAX);
                let after = end_no
                    .saturating_add(context)
                    .min(next_line.saturating_sub(1))
                    .min(source_lines.len());
                for number in (end_no + 1)..=after {
                    push_context(&mut entry, number);
                }
                printed = printed.max(after);
            }
            if rendered.body.len() + block.len() + entry.len() > budget {
                rendered.limit_reached = true;
                break;
            }
            block.push_str(&entry);
            kept += 1;
        }
        if kept > 0 {
            rendered.body.push_str(&block);
            rendered.match_count += kept;
            rendered.file_count += 1;
        }
        if rendered.limit_reached {
            break;
        }
    }
    rendered
}
// rewrite 미리보기 diff (무쓰기)
fn render_rewrite(
    matches: &[AstMatch],
    context: Option<usize>,
    max_results: usize,
    budget: usize,
    version: &str,
) -> Result<Rendered, String> {
    let spec = DiffSpec {
        context: context.unwrap_or(DIFF_CONTEXT),
        ..DiffSpec::default()
    };
    let mut rendered = Rendered {
        body: String::new(),
        match_count: 0,
        file_count: 0,
        limit_reached: false,
        nested_skipped: 0,
    };
    for group in matches.chunk_by(|left, right| left.file == right.file) {
        let Some(head) = group.first() else {
            continue;
        };
        let room = max_results.saturating_sub(rendered.match_count);
        if room == 0 {
            rendered.limit_reached = true;
            break;
        }
        let (outermost, nested) = keep_outermost(group)?;
        rendered.nested_skipped += nested;
        let applied = &outermost[..outermost.len().min(room)];
        let bytes = std::fs::read(&head.file).map_err(|error| {
            format!(
                "Failed to read {} for the rewrite preview: {error}",
                head.file
            )
        })?;
        let original = String::from_utf8(bytes).map_err(|_| {
            format!(
                "{} is not valid UTF-8; ast-grep rewrite offsets cannot be applied",
                head.file
            )
        })?;

        // 바이트 오프셋 적용
        let mut rewritten = String::with_capacity(original.len());
        let mut cursor = 0usize;
        for found in applied {
            let Some(replacement) = &found.replacement else {
                return Err(format!(
                    "ast-grep {version} --json=stream output lacks the field /replacement for a rewrite match"
                ));
            };
            let kept = original
                .get(cursor..found.start)
                .filter(|_| original.get(found.start..found.end) == Some(found.text.as_str()));
            let Some(kept) = kept else {
                return Err(format!(
                    "{} changed after ast-grep read it (bytes {}..{} no longer hold the match at line {}); rerun",
                    head.file,
                    found.start,
                    found.end,
                    found.line + 1
                ));
            };
            rewritten.push_str(kept);
            rewritten.push_str(replacement);
            cursor = found.end;
        }
        rewritten.push_str(original.get(cursor..).unwrap_or_default());
        let summary = render_unified_diff(&original, &rewritten, &head.file, &head.file, &spec);
        let block = if summary.identical {
            format!("{}\n[rewrite leaves this file unchanged]\n", head.file)
        } else {
            summary.unified
        };
        if !rendered.body.is_empty() && rendered.body.len() + block.len() > budget {
            rendered.limit_reached = true;
            break;
        }
        rendered.body.push_str(&block);
        if !rendered.body.ends_with('\n') {
            rendered.body.push('\n');
        }
        rendered.match_count += applied.len();
        rendered.file_count += 1;
        if applied.len() < outermost.len() {
            rendered.limit_reached = true;
            break;
        }
    }
    Ok(rendered)
}
// 바깥 매치 우선 (ast-grep -U 동일)
fn keep_outermost(group: &[AstMatch]) -> Result<(Vec<&AstMatch>, usize), String> {
    let mut outermost: Vec<&AstMatch> = Vec::with_capacity(group.len());
    let mut nested = 0usize;
    let mut cursor = 0usize;
    for found in group {
        if found.start >= cursor {
            cursor = found.end;
            outermost.push(found);
        } else if found.end <= cursor {
            nested += 1;
        } else {
            return Err(format!(
                "ast-grep rewrite produced partially overlapping edits in {} at line {} (bytes {}..{} cross {cursor}); narrow the pattern",
                found.file,
                found.line + 1,
                found.start,
                found.end
            ));
        }
    }
    Ok((outermost, nested))
}
// 표시용 첫 줄 절단
fn clip_display(text: &str) -> String {
    let mut lines = text.split('\n');
    let first = lines
        .next()
        .unwrap_or("")
        .trim_end_matches('\r')
        .trim_start_matches('\u{feff}');
    let more_lines = lines.next().is_some();
    let mut clipped: String = first.chars().take(CLIP_CHARS).collect();
    if more_lines || clipped.len() < first.len() {
        clipped.push('…');
    }
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::external::ast_grep_program;
    use std::path::PathBuf;
    use std::sync::OnceLock;

    // ast-grep 부재 시 건너뜀 (1회 판정)
    fn ast_grep_ready() -> bool {
        static READY: OnceLock<bool> = OnceLock::new();
        *READY.get_or_init(|| {
            let ready = ast_grep_version().is_ok();
            if !ready {
                eprintln!("ast-grep not on PATH; skipping ast-search CLI tests");
            }
            ready
        })
    }
    fn temp_dir(prefix: &str) -> PathBuf {
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
    fn search(item: Value) -> (bool, String, Value) {
        let result = handle_ast_search(&json!({ "items": [item] }));
        let text = result.content[0]["text"].as_str().unwrap().to_string();
        let data = result.structured.unwrap()["results"][0]["data"].clone();
        (result.is_error, text, data)
    }
    const FIXTURE: &str = "fn main() {\n    // a.unwrap() in a comment\n    let s = \"b.unwrap()\";\n    let v = c.unwrap();\n}\n";

    // 1. 코드만 매치 ----------------------------------------------------------
    #[test]
    fn matches_code_but_not_comments_or_strings() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-code");
        let file = dir.join("main.rs");
        std::fs::write(&file, FIXTURE).unwrap();
        let (is_error, text, data) = search(json!({
            "path": dir.display().to_string(), "pattern": "$A.unwrap()", "lang": "rust"
        }));
        assert!(!is_error, "{text}");
        assert_eq!(data["totalCount"], 1, "{text}");
        assert_eq!(data["fileCount"], 1);
        assert_eq!(data["backend"], "path-ast-grep");
        assert!(data["astGrepVersion"].as_str().is_some());
        assert!(text.contains("4:13: c.unwrap()"), "{text}");
        // 매치 줄에 보이는 $A 생략
        assert!(!text.contains("$A ="), "{text}");
        assert!(!text.contains("b.unwrap"), "{text}");
        std::fs::remove_dir_all(dir).ok();
    }

    // 2. 메타변수·문맥 줄 -----------------------------------------------------
    #[test]
    fn multi_metavariables_and_context_lines() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-meta");
        let file = dir.join("lib.rs");
        std::fs::write(
            &file,
            "fn f() {\n    let x = 1;\n    foo(\n        1,\n        2,\n    );\n    let y = 2;\n}\n",
        )
        .unwrap();
        let (is_error, text, _) = search(json!({
            "path": file.display().to_string(), "pattern": "foo($$$ARGS)", "contextLines": 1
        }));
        assert!(!is_error, "{text}");
        assert!(
            text.contains("2-    let x = 1;\n3:5: foo(…\n  $$$ARGS = 1,…\n7-    let y = 2;"),
            "{text}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    // 3. 한글 바이트 오프셋 ---------------------------------------------------
    #[test]
    fn hangul_edit_offsets_slice_the_original() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-hangul");
        let file = dir.join("ko.rs");
        let source = "fn 한글() {\n    let 값 = 이름.unwrap();\n}\n";
        std::fs::write(&file, source).unwrap();
        let edits = collect_ast_edits(&AstRewriteSpec {
            path: &file,
            pattern: "$A.unwrap()",
            rewrite: "$A?",
            lang: Some("rust"),
            timeout_ms: 20_000,
        })
        .unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(&source[edits[0].start..edits[0].end], edits[0].original);
        assert_eq!(edits[0].original, "이름.unwrap()");
        assert_eq!(edits[0].replacement, "이름?");
        assert_eq!(edits[0].line, 2);
        std::fs::remove_dir_all(dir).ok();
    }

    // 4. rewrite 미리보기 (무쓰기) --------------------------------------------
    #[test]
    fn rewrite_preview_renders_diff_without_writing() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-rewrite");
        let file = dir.join("main.rs");
        std::fs::write(&file, FIXTURE).unwrap();
        let before = std::fs::read(&file).unwrap();
        let (is_error, text, data) = search(json!({
            "path": dir.display().to_string(), "pattern": "$A.unwrap()", "rewrite": "$A?", "lang": "rust"
        }));
        assert!(!is_error, "{text}");
        assert_eq!(data["rewrite"], true);
        assert_eq!(data["totalCount"], 1);
        assert!(
            text.contains("-    let v = c.unwrap();\n+    let v = c?;"),
            "{text}"
        );
        assert_eq!(std::fs::read(&file).unwrap(), before);
        std::fs::remove_dir_all(dir).ok();
    }

    // 5. maxResults 절단 ------------------------------------------------------
    #[test]
    fn max_results_sets_limit_reached() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-limit");
        std::fs::write(dir.join("a.rs"), "fn a() { x.unwrap(); y.unwrap(); }\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn b() { z.unwrap(); }\n").unwrap();
        let (is_error, text, data) = search(json!({
            "path": dir.display().to_string(), "pattern": "$A.unwrap()", "maxResults": 2
        }));
        assert!(!is_error, "{text}");
        assert_eq!(data["totalCount"], 2);
        assert_eq!(data["limitReached"], true);
        assert!(text.contains("2 of 3 matches shown"), "{text}");
        // 파일·위치 정렬 (a.rs 먼저)
        assert!(
            text.contains("1:10: x.unwrap()\n1:22: y.unwrap()"),
            "{text}"
        );
        assert!(!text.contains("b.rs"), "{text}");
        std::fs::remove_dir_all(dir).ok();
    }

    // 6. 오류 계약 ------------------------------------------------------------
    #[test]
    fn missing_path_and_bad_pattern_are_errors() {
        let missing = std::env::temp_dir().join("rfm-ast-missing-path-xyz");
        let (is_error, text, _) = search(json!({
            "path": missing.display().to_string(), "pattern": "$A"
        }));
        assert!(is_error);
        assert!(text.contains("Path does not exist"), "{text}");
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-bad");
        let file = dir.join("main.rs");
        std::fs::write(&file, FIXTURE).unwrap();
        let (is_error, text, _) = search(json!({
            "path": file.display().to_string(), "pattern": "let = ;", "lang": "rust"
        }));
        assert!(is_error);
        assert!(
            text.contains("Cannot parse query as a valid pattern"),
            "{text}"
        );
        let (is_error, text, _) = search(json!({
            "path": file.display().to_string(), "pattern": "a.unwrap(", "lang": "rust"
        }));
        assert!(is_error);
        assert!(text.contains("did not parse cleanly"), "{text}");
        let (is_error, text, _) = search(json!({
            "path": file.display().to_string(), "pattern": "$A", "lang": "rustx"
        }));
        assert!(is_error);
        assert!(text.contains("rustx"), "{text}");
        std::fs::remove_dir_all(dir).ok();
    }

    // 7. 단일 파일 미파싱 거절 ------------------------------------------------
    #[test]
    fn unparsed_single_file_is_an_error() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-unparsed");
        let text_file = dir.join("notes.txt");
        std::fs::write(&text_file, FIXTURE).unwrap();
        let (is_error, text, _) = search(json!({
            "path": text_file.display().to_string(), "pattern": "$A.unwrap()", "lang": "rust"
        }));
        assert!(is_error);
        assert!(text.contains("maps to no language"), "{text}");
        let euc_file = dir.join("euc.rs");
        std::fs::write(
            &euc_file,
            b"fn e() { // \xC7\xD1\xB1\xDB\n    k.unwrap();\n}\n",
        )
        .unwrap();
        let (is_error, text, _) = search(json!({
            "path": euc_file.display().to_string(), "pattern": "$A.unwrap()", "lang": "rust"
        }));
        assert!(is_error);
        assert!(text.contains("not valid UTF-8"), "{text}");
        std::fs::remove_dir_all(dir).ok();
    }

    // 8. CRLF 오프셋 ----------------------------------------------------------
    #[test]
    fn crlf_offsets_index_raw_bytes() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-crlf");
        let file = dir.join("crlf.rs");
        let source = "fn f() {\r\n    let x = a.unwrap();\r\n    let y = b.unwrap();\r\n}\r\n";
        std::fs::write(&file, source).unwrap();
        let edits = collect_ast_edits(&AstRewriteSpec {
            path: &file,
            pattern: "$A.unwrap()",
            rewrite: "$A?",
            lang: None,
            timeout_ms: 20_000,
        })
        .unwrap();
        assert_eq!(edits.len(), 2);
        for edit in &edits {
            assert_eq!(&source[edit.start..edit.end], edit.original);
        }
        assert_eq!((edits[0].line, edits[1].line), (2, 3));
        let (is_error, text, _) = search(json!({
            "path": file.display().to_string(), "pattern": "$A.unwrap()", "rewrite": "$A?"
        }));
        assert!(!is_error, "{text}");
        assert!(!text.contains('\r'), "{text:?}");
        assert!(text.contains("+    let y = b?;"), "{text}");
        std::fs::remove_dir_all(dir).ok();
    }

    // 9. 표시 절단·ANSI 제거 --------------------------------------------------
    #[test]
    fn clip_display_and_ansi_strip() {
        assert_eq!(clip_display("call(\r\n  1,\r\n)"), "call(…");
        assert_eq!(clip_display("\u{feff}fn a()"), "fn a()");
        let long = "가".repeat(CLIP_CHARS + 5);
        assert_eq!(clip_display(&long).chars().count(), CLIP_CHARS + 1);
        assert_eq!(strip_ansi("\u{1b}[31mError\u{1b}[0m: x"), "Error: x");
    }

    // 10. 중첩 매치 바깥 우선 -------------------------------------------------
    #[test]
    fn nested_matches_rewrite_outermost_like_update_all() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-nested");
        let file = dir.join("nest.rs");
        let source = "fn n() { a.unwrap().unwrap(); b.unwrap(); }\n";
        std::fs::write(&file, source).unwrap();
        let edits = collect_ast_edits(&AstRewriteSpec {
            path: &file,
            pattern: "$A.unwrap()",
            rewrite: "$A?",
            lang: None,
            timeout_ms: 20_000,
        })
        .unwrap();
        let replaced: Vec<&str> = edits.iter().map(|edit| edit.replacement.as_str()).collect();
        assert_eq!(replaced, ["a.unwrap()?", "b?"]);
        let (is_error, text, data) = search(json!({
            "path": file.display().to_string(), "pattern": "$A.unwrap()", "rewrite": "$A?"
        }));
        assert!(!is_error, "{text}");
        assert_eq!(data["nestedSkipped"], 1);
        assert!(text.contains("+fn n() { a.unwrap()?; b?; }"), "{text}");
        std::fs::remove_dir_all(dir).ok();
    }

    // 11. BOM 첫 줄 열 보정 ---------------------------------------------------
    #[test]
    fn bom_does_not_shift_first_line_columns() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-bom");
        let file = dir.join("bom.rs");
        std::fs::write(&file, "\u{feff}fn b() { k.unwrap(); }\n").unwrap();
        let (is_error, text, _) = search(json!({
            "path": file.display().to_string(), "pattern": "$A.unwrap()"
        }));
        assert!(!is_error, "{text}");
        assert!(text.contains("1:10: k.unwrap()"), "{text}");
        std::fs::remove_dir_all(dir).ok();
    }

    // 12. 문맥 줄 루프 상한 ---------------------------------------------------
    #[test]
    fn huge_context_lines_stay_bounded_by_the_file() {
        let dir = temp_dir("rfm-ast-huge-context");
        let file = dir.join("main.rs");
        std::fs::write(&file, FIXTURE).unwrap();
        let path = file.display().to_string();
        let found = move || AstMatch {
            file: path.clone(),
            start: 0,
            end: 0,
            line: 3,
            end_line: 3,
            column: 12,
            text: "c.unwrap()".to_string(),
            replacement: None,
            metavars: Vec::new(),
        };
        let expected = render_matches(&[found()], 10, usize::MAX, usize::MAX, false).body;
        for context in [usize::MAX, 1usize << 62] {
            let (sender, receiver) = std::sync::mpsc::channel();
            let matches = vec![found()];
            std::thread::spawn(move || {
                let rendered = render_matches(&matches, context, usize::MAX, usize::MAX, false);
                let _ = sender.send(rendered.body);
            });
            let body = receiver
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("render_matches must finish for a huge contextLines");
            assert_eq!(body, expected, "context {context}");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    // 13. 비정상 ast-grep 좌표 거절 -------------------------------------------
    #[test]
    fn malformed_match_coordinates_are_errors() {
        let record = |start: u64, end: u64, line: u64| {
            json!({
                "file": "x.rs",
                "text": "a",
                "range": {
                    "byteOffset": { "start": start, "end": end },
                    "start": { "line": line, "column": 0 },
                    "end": { "line": line, "column": 1 }
                }
            })
            .to_string()
        };
        assert!(parse_match(&record(0, 1, 0), "0.51.1").is_ok());
        assert!(parse_match(&record(2, 1, 0), "0.51.1").is_err());
        assert!(parse_match(&record(0, 1, u64::MAX), "0.51.1").is_err());
    }

    // 14. 거대 contextLines 핸들러 경로 ---------------------------------------
    #[test]
    fn huge_context_lines_through_the_handler() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-handler-context");
        let file = dir.join("main.rs");
        std::fs::write(&file, FIXTURE).unwrap();
        let path = file.display().to_string();
        for context in [u64::MAX, 1u64 << 63] {
            let (is_error, text, _) = search(json!({
                "path": path, "pattern": "$A.unwrap()", "rewrite": "$A?", "lang": "rust", "contextLines": context
            }));
            assert!(!is_error, "{text}");
            assert!(
                text.contains("-    let v = c.unwrap();\n+    let v = c?;"),
                "{text}"
            );
        }
        let (is_error, text, _) = search(json!({
            "path": path, "pattern": "$A.unwrap()", "lang": "rust", "contextLines": u64::MAX
        }));
        assert!(!is_error, "{text}");
        assert!(
            text.contains("3-    let s = \"b.unwrap()\";\n4:13: c.unwrap()\n5-}"),
            "{text}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    // 15. 거대 timeout_ms -----------------------------------------------------
    #[test]
    fn huge_timeout_ms_runs_normally() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-huge-timeout");
        let file = dir.join("main.rs");
        std::fs::write(&file, FIXTURE).unwrap();
        let (is_error, text, data) = search(json!({
            "path": file.display().to_string(), "pattern": "$A.unwrap()", "timeout_ms": u64::MAX
        }));
        assert!(!is_error, "{text}");
        assert_eq!(data["totalCount"], 1, "{text}");
        std::fs::remove_dir_all(dir).ok();
    }

    // 16. 파이프 상한 rewrite 거절 --------------------------------------------
    #[test]
    fn capped_rewrite_output_is_refused() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-capped");
        let file = dir.join("min.rs");
        let padding = "a".repeat(2 * 1024 * 1024);
        let calls = "x.unwrap(); ".repeat(12);
        std::fs::write(&file, format!("fn f() {{ /*{padding}*/ {calls}}}\n")).unwrap();
        let edits = collect_ast_edits(&AstRewriteSpec {
            path: &file,
            pattern: "$A.unwrap()",
            rewrite: "$A?",
            lang: None,
            timeout_ms: 60_000,
        });
        let error = edits.expect_err("a capped edit set must be refused");
        assert!(error.contains("pipe cap"), "{error}");
        let (is_error, text, data) = search(json!({
            "path": file.display().to_string(), "pattern": "$A.unwrap()"
        }));
        assert!(!is_error, "{text}");
        assert_eq!(data["limitReached"], true, "{text}");
        std::fs::remove_dir_all(dir).ok();
    }

    // 17. 시간 초과 부분 결과 -------------------------------------------------
    #[test]
    fn timed_out_search_keeps_partial_matches() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-timeout");
        let filler = "    let w = a + b * c;\n".repeat(9000);
        let body = format!("fn f() {{\n    let v = x.unwrap();\n{filler}}}\n");
        for index in 0..200 {
            std::fs::write(dir.join(format!("f{index:03}.rs")), &body).unwrap();
        }
        let root = dir.display().to_string();
        // 전체 실행 소요 기준 기한
        let started = std::time::Instant::now();
        let (is_error, text, data) = search(json!({
            "path": root, "pattern": "$A.unwrap()", "lang": "rust"
        }));
        let full_ms = started.elapsed().as_millis() as u64;
        assert!(!is_error, "{text}");
        assert_eq!(data["totalCount"], 200, "{text}");
        assert!(data.get("timedOut").is_none(), "{text}");
        let budget = (full_ms / 3).max(1);
        let (is_error, text, data) = search(json!({
            "path": root, "pattern": "$A.unwrap()", "lang": "rust", "timeout_ms": budget
        }));
        assert!(!is_error, "{budget}ms of {full_ms}ms: {text}");
        assert_eq!(data["timedOut"], true, "{budget}ms of {full_ms}ms: {text}");
        let partial = data["totalCount"].as_u64().unwrap();
        assert!(partial > 0 && partial < 200, "{partial} matches: {text}");
        assert!(
            text.contains("\n[search stopped at the time budget; results are partial"),
            "{text}"
        );
        // 파일 제목 정렬 순서
        let headings: Vec<&str> = text.lines().filter(|line| line.ends_with(".rs")).collect();
        assert_eq!(headings.len() as u64, partial, "{text}");
        assert!(headings.is_sorted(), "{headings:?}");
        let (is_error, text, _) = search(json!({
            "path": root, "pattern": "$A.unwrap()", "lang": "rust", "timeout_ms": 1
        }));
        assert!(is_error, "{text}");
        assert!(
            text.contains("timed out after 1ms with no matches yet"),
            "{text}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    // 18. npm 배치·환경변수 실행 파일 -----------------------------------------
    #[test]
    fn npm_layout_and_env_override_run_ast_search() {
        if !ast_grep_ready() {
            return;
        }
        let exe_name = if cfg!(windows) {
            "ast-grep.exe"
        } else {
            "ast-grep"
        };
        let real = ast_grep_program().unwrap();
        let dir = temp_dir("rfm-ast-npm");
        let prefix = dir.join("npm");
        let package = prefix.join("node_modules").join("@ast-grep").join("cli");
        std::fs::create_dir_all(&package).unwrap();
        let native = package.join(exe_name);
        if std::fs::hard_link(real, &native).is_err() {
            std::fs::copy(real, &native).unwrap();
        }
        // npm 전역 래퍼 (실행 안 함)
        std::fs::write(
            prefix.join("ast-grep.cmd"),
            "@ECHO off\r\nnode \"%~dp0\\node_modules\\@ast-grep\\cli\\ast-grep\" %*\r\n",
        )
        .unwrap();
        let source = dir.join("main.rs");
        std::fs::write(&source, FIXTURE).unwrap();
        let missing = dir.join("missing").join(exe_name);
        // 런타임 DLL 경로 유지, ast-grep 경로 제외
        let kept: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .filter(|dir| !dir.join(exe_name).is_file() && !dir.join("ast-grep.cmd").is_file())
            .collect();
        let bare = std::env::join_paths(&kept).unwrap();
        let npm_path = std::env::join_paths(std::iter::once(prefix.clone()).chain(kept)).unwrap();
        // 서버 cwd 기준 상대 지정값
        let relative = Path::new("npm")
            .join("node_modules")
            .join("@ast-grep")
            .join("cli")
            .join(exe_name);
        let shim = prefix.join("ast-grep.cmd");
        let mut cases = vec![
            (&bare, Some(native.as_path()), None, "found"),
            (
                &bare,
                Some(relative.as_path()),
                Some(dir.as_path()),
                "found",
            ),
        ];
        if cfg!(windows) {
            cases.push((&npm_path, None, None, "found"));
            // npm 래퍼 지정값
            cases.push((&bare, Some(shim.as_path()), None, "found"));
        }
        cases.push((&bare, Some(missing.as_path()), None, "missing"));
        for (path_dir, explicit, cwd, expect) in cases {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            if let Some(cwd) = cwd {
                child.current_dir(cwd);
            }
            child
                .args([
                    "--exact",
                    "tools::search::ast::tests::ast_search_child_run",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("PATH", path_dir)
                .env("RFM_AST_CHILD_FILE", &source)
                .env("RFM_AST_CHILD_EXPECT", expect)
                .env("RFM_AST_CHILD_PROGRAM", &native);
            match explicit {
                Some(program) => child.env("RUST_FS_MCP_AST_GREP", program),
                None => child.env_remove("RUST_FS_MCP_AST_GREP"),
            };
            let output = child.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{expect} with PATH {path_dir:?} and override {explicit:?}:\n{stdout}\n{stderr}"
            );
        }
        std::fs::remove_dir_all(dir).ok();
    }
    #[test]
    #[ignore = "child process of npm_layout_and_env_override_run_ast_search"]
    fn ast_search_child_run() {
        let file = std::env::var("RFM_AST_CHILD_FILE").unwrap();
        let expect = std::env::var("RFM_AST_CHILD_EXPECT").unwrap();
        let (is_error, text, data) = search(json!({
            "path": file, "pattern": "$A.unwrap()", "lang": "rust"
        }));
        if expect == "missing" {
            assert!(is_error, "{text}");
            assert!(text.contains("RUST_FS_MCP_AST_GREP"), "{text}");
            return;
        }
        assert!(!is_error, "{text}");
        assert_eq!(data["totalCount"], 1, "{text}");
        // 해석 경로 = 가짜 npm 배치의 네이티브 바이너리 (절대 경로)
        let expected = PathBuf::from(std::env::var_os("RFM_AST_CHILD_PROGRAM").unwrap());
        let program = ast_grep_program().unwrap();
        assert!(program.is_absolute(), "{}", program.display());
        assert_eq!(
            std::fs::canonicalize(program).unwrap(),
            std::fs::canonicalize(expected).unwrap()
        );
    }
}
