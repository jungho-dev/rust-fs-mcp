//! ast.rs
//! tools::search::ast
//!
//! Rewrite edits from the ast-grep CLI (--json=stream output) that file-replace mode "ast" applies.
//! Byte offsets are raw UTF-8 file offsets (BOM and CRLF included); edit lines are reported 1-based.
//! The file's folder is the ast-grep cwd, so the project's sgconfig.yml (languageGlobs, customLanguages) applies.
//! Matches are parsed while ast-grep streams; a timeout or a capped output refuses the whole edit set (all-or-nothing).
//!

use crate::core::external::{ExternalTool, MAX_PIPE_BYTES, ast_grep_version, run_external_lines};
use serde::Deserialize;
use std::borrow::Cow;
use std::cmp::Reverse;
use std::path::Path;

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
    let outermost = keep_outermost(&matches)?;
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

// 2. ast-grep runner ----------------------------------------------------------
struct AstMatch {
    file: String,
    start: usize,
    end: usize,
    // 0부터 (ast-grep 원본)
    line: usize,
    text: String,
    replacement: Option<String>,
}
struct AstRun {
    matches: Vec<AstMatch>,
    // 파이프 상한 도달 (부분 결과)
    capped: bool,
    // 기한 도달 (부분 결과)
    timed_out: bool,
    scanned: Option<usize>,
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
                    if key.trim() == "scannedFileCount" {
                        run.scanned = count.trim().parse::<usize>().ok();
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
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordRange {
    byte_offset: ByteSpan,
    start: LinePosition,
    end: LinePosition,
}
#[derive(Clone, Copy, Deserialize)]
struct ByteSpan {
    start: usize,
    end: usize,
}
#[derive(Deserialize)]
struct LinePosition {
    line: usize,
}
// --json=stream 한 줄 파싱
fn parse_match(line: &str, version: &str) -> Result<AstMatch, String> {
    let record: StreamRecord<'_> = serde_json::from_str(line).map_err(|error| {
        format!(
            "ast-grep {version} printed malformed --json=stream output ({error}); validated with ast-grep 0.51.1"
        )
    })?;
    let ByteSpan { start, end } = record.range.byte_offset;
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
    // 역전·overflow 좌표 거절
    let line = record.range.start.line;
    let end_line = record.range.end.line;
    if start > end || end_line < line || end_line == usize::MAX {
        return Err(format!(
            "ast-grep {version} --json=stream output has an invalid range (bytes {start}..{end}, lines {line}..{end_line}); validated with ast-grep 0.51.1"
        ));
    }
    Ok(AstMatch {
        file: record.file.into_owned(),
        start,
        end,
        line,
        text: record.text.into_owned(),
        replacement,
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
// 바깥 매치 우선 (ast-grep -U 동일)
fn keep_outermost(group: &[AstMatch]) -> Result<Vec<&AstMatch>, String> {
    let mut outermost: Vec<&AstMatch> = Vec::with_capacity(group.len());
    let mut cursor = 0usize;
    for found in group {
        if found.start >= cursor {
            cursor = found.end;
            outermost.push(found);
        } else if found.end > cursor {
            return Err(format!(
                "ast-grep rewrite produced partially overlapping edits in {} at line {} (bytes {}..{} cross {cursor}); narrow the pattern",
                found.file,
                found.line + 1,
                found.start,
                found.end
            ));
        }
    }
    Ok(outermost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::external::ast_grep_program;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::OnceLock;

    // ast-grep 부재 시 건너뜀 (1회 판정)
    fn ast_grep_ready() -> bool {
        static READY: OnceLock<bool> = OnceLock::new();
        *READY.get_or_init(|| {
            let ready = ast_grep_version().is_ok();
            if !ready {
                eprintln!("ast-grep not on PATH; skipping ast rewrite CLI tests");
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
    const FIXTURE: &str = "fn main() {\n    // a.unwrap() in a comment\n    let s = \"b.unwrap()\";\n    let v = c.unwrap();\n}\n";

    // 1. 코드만 매치 ----------------------------------------------------------
    #[test]
    fn edits_cover_code_but_not_comments_or_strings() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-code");
        let file = dir.join("main.rs");
        std::fs::write(&file, FIXTURE).unwrap();
        let edits = collect_ast_edits(&AstRewriteSpec {
            path: &file,
            pattern: "$A.unwrap()",
            rewrite: "$A?",
            lang: Some("rust"),
            timeout_ms: 20_000,
        })
        .unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].original, "c.unwrap()");
        assert_eq!(edits[0].replacement, "c?");
        assert_eq!(edits[0].line, 4);
        std::fs::remove_dir_all(dir).ok();
    }

    // 2. 한글 바이트 오프셋 ---------------------------------------------------
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

    // 3. 패턴·언어 오류 계약 --------------------------------------------------
    #[test]
    fn bad_pattern_and_lang_are_errors() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-bad");
        let file = dir.join("main.rs");
        std::fs::write(&file, FIXTURE).unwrap();
        for (pattern, lang, expected) in [
            ("let = ;", "rust", "Cannot parse query as a valid pattern"),
            ("a.unwrap(", "rust", "did not parse cleanly"),
            ("$A", "rustx", "rustx"),
        ] {
            let error = collect_ast_edits(&AstRewriteSpec {
                path: &file,
                pattern,
                rewrite: "x",
                lang: Some(lang),
                timeout_ms: 20_000,
            })
            .expect_err(pattern);
            assert!(error.contains(expected), "{pattern}: {error}");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    // 4. 단일 파일 미파싱 거절 ------------------------------------------------
    #[test]
    fn unparsed_single_file_is_an_error() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-unparsed");
        let text_file = dir.join("notes.txt");
        std::fs::write(&text_file, FIXTURE).unwrap();
        let euc_file = dir.join("euc.rs");
        std::fs::write(
            &euc_file,
            b"fn e() { // \xC7\xD1\xB1\xDB\n    k.unwrap();\n}\n",
        )
        .unwrap();
        for (path, expected) in [
            (&text_file, "maps to no language"),
            (&euc_file, "not valid UTF-8"),
        ] {
            let error = collect_ast_edits(&AstRewriteSpec {
                path,
                pattern: "$A.unwrap()",
                rewrite: "$A?",
                lang: Some("rust"),
                timeout_ms: 20_000,
            })
            .expect_err(expected);
            assert!(error.contains(expected), "{error}");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    // 5. CRLF 오프셋 ----------------------------------------------------------
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
        std::fs::remove_dir_all(dir).ok();
    }

    // 6. ANSI 제거 ------------------------------------------------------------
    #[test]
    fn ansi_color_codes_are_stripped() {
        assert_eq!(strip_ansi("\u{1b}[31mError\u{1b}[0m: x"), "Error: x");
    }

    // 7. 중첩 매치 바깥 우선 --------------------------------------------------
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
        std::fs::remove_dir_all(dir).ok();
    }

    // 8. 비정상 ast-grep 좌표 거절 --------------------------------------------
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

    // 9. 거대 timeout_ms ------------------------------------------------------
    #[test]
    fn huge_timeout_ms_runs_normally() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-huge-timeout");
        let file = dir.join("main.rs");
        std::fs::write(&file, FIXTURE).unwrap();
        let edits = collect_ast_edits(&AstRewriteSpec {
            path: &file,
            pattern: "$A.unwrap()",
            rewrite: "$A?",
            lang: None,
            timeout_ms: u64::MAX,
        })
        .unwrap();
        assert_eq!(edits.len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    // 10. 파이프 상한 rewrite 거절 --------------------------------------------
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
        std::fs::remove_dir_all(dir).ok();
    }

    // 11. 시간 초과 rewrite 거절 ----------------------------------------------
    #[test]
    fn timed_out_rewrite_is_refused() {
        if !ast_grep_ready() {
            return;
        }
        let dir = temp_dir("rfm-ast-timeout");
        let file = dir.join("slow.rs");
        let filler = "    let w = a + b * c;\n".repeat(9000);
        std::fs::write(
            &file,
            format!("fn f() {{\n    let v = x.unwrap();\n{filler}}}\n"),
        )
        .unwrap();
        let edits = collect_ast_edits(&AstRewriteSpec {
            path: &file,
            pattern: "$A.unwrap()",
            rewrite: "$A?",
            lang: Some("rust"),
            timeout_ms: 1,
        });
        let error = edits.expect_err("a timed-out edit set must be refused");
        assert!(
            error.contains("timed out after 1ms") && error.contains("nothing was applied"),
            "{error}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    // 12. npm 배치·환경변수 실행 파일 -----------------------------------------
    #[test]
    fn npm_layout_and_env_override_resolve_ast_grep() {
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
                    "tools::search::ast::tests::ast_rewrite_child_run",
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
    #[ignore = "child process of npm_layout_and_env_override_resolve_ast_grep"]
    fn ast_rewrite_child_run() {
        let file = PathBuf::from(std::env::var_os("RFM_AST_CHILD_FILE").unwrap());
        let expect = std::env::var("RFM_AST_CHILD_EXPECT").unwrap();
        let edits = collect_ast_edits(&AstRewriteSpec {
            path: &file,
            pattern: "$A.unwrap()",
            rewrite: "$A?",
            lang: Some("rust"),
            timeout_ms: 20_000,
        });
        if expect == "missing" {
            let error = edits.expect_err("a missing override must fail");
            assert!(error.contains("RUST_FS_MCP_AST_GREP"), "{error}");
            return;
        }
        assert_eq!(edits.unwrap().len(), 1);
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
