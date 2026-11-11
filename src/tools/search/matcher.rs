//! matcher.rs
//! tools::search::matcher
//!
//! Linear (regex) and backtracking (fancy-regex) grep matchers plus smart-case pattern analysis.
//!

use grep_matcher::{LineTerminator, Match, Matcher, NoCaptures, NoError};
use regex_syntax::hir::{Class, HirKind, Look};

const FANCY_BACKTRACK_LIMIT: usize = 1_000_000;

// 1. Smart-case analysis ------------------------------------------------------
// smart-case 대문자 판정 (이스케이프·그룹명 제외)
pub(super) fn has_uppercase_literal(pattern: &str, literal: bool) -> bool {
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

// 2. regex(bytes) -> grep Matcher adapter -------------------------------------
// rg와 동일하게 라인 지향 ^/$ 매칭을 위해 (?m)을 상시 켠다. 기본에서는 `.`이 개행을
// 매치하지 않아 매치가 한 줄 안에 갇히고, multiline 옵션이 (?s)로 줄 경계를 연다.
#[derive(Clone, Debug)]
pub(crate) struct RegexAdapter {
    pub(crate) regex: regex::bytes::Regex,
    // 줄 단위 검색의 CRLF 종결 CR 제외
    pub(super) trims_cr: bool,
    // 버퍼 단위 탐색 적격 (fast line path)
    pub(super) line_bound: bool,
}
impl Matcher for RegexAdapter {
    type Captures = NoCaptures;
    type Error = NoError;
    fn find_at(&self, haystack: &[u8], at: usize) -> Result<Option<Match>, NoError> {
        let haystack = match haystack.split_last() {
            Some((b'\r', line)) if self.trims_cr && at < haystack.len() => line,
            _ => haystack,
        };
        Ok(self
            .regex
            .find_at(haystack, at)
            .map(|found| Match::new(found.start(), found.end())))
    }
    fn new_captures(&self) -> Result<NoCaptures, NoError> {
        Ok(NoCaptures::new())
    }
    fn line_terminator(&self) -> Option<LineTerminator> {
        self.line_bound.then(|| LineTerminator::byte(b'\n'))
    }
}
// linear engine이 문법으로만 인지하고 실행은 못 하는 lookaround/backreference 전용 backtracking
// 어댑터. find_from_pos는 pos 이전 텍스트를 lookbehind로 참조하므로 grep의 재개(find_at) 계약과
// 정확히 맞고, 비UTF-8 조각은 매치 없음으로 흘려보낸다(binary_detection이 NUL에서 이미 중단).
#[derive(Clone)]
pub(crate) struct FancyAdapter {
    pub(crate) regex: fancy_regex::Regex,
    // 줄 단위 검색의 CRLF 종결 CR 제외
    pub(super) trims_cr: bool,
}
impl Matcher for FancyAdapter {
    type Captures = NoCaptures;
    type Error = NoError;
    fn find_at(&self, haystack: &[u8], at: usize) -> Result<Option<Match>, NoError> {
        let haystack = match haystack.split_last() {
            Some((b'\r', line)) if self.trims_cr && at < haystack.len() => line,
            _ => haystack,
        };
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
    fn line_terminator(&self) -> Option<LineTerminator> {
        match self {
            SearchMatcher::Linear(matcher) => matcher.line_terminator(),
            SearchMatcher::Fancy(_) => None,
        }
    }
}
// 3. Pattern compilation ------------------------------------------------------
// 패턴 컴파일 옵션: literal=rg -F, word_match=rg -w, multiline=rg -U(+dot-all) 대응.
pub(super) struct PatternOpts {
    pub(super) ignore_case: bool,
    pub(super) literal: bool,
    pub(super) multiline: bool,
    pub(super) word_match: bool,
}
// wordMatch 래퍼 의존 문법 차단 (단독 검증 우선)
pub(super) fn build_matcher(
    pattern: &str,
    opts: &PatternOpts,
    force_literal: bool,
) -> Result<RegexAdapter, regex::Error> {
    let escaped = opts.literal || force_literal;
    let body = if escaped {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let compile = |source: &str| {
        regex::bytes::RegexBuilder::new(source)
            .case_insensitive(opts.ignore_case)
            .multi_line(true)
            .crlf(true)
            .dot_matches_new_line(opts.multiline)
            .build()
    };
    // 컴파일 소스 기준 적격 판정
    let adapt = |regex: regex::bytes::Regex, source: &str| RegexAdapter {
        regex,
        trims_cr: !opts.multiline,
        line_bound: !opts.multiline && suits_buffer_search(source, opts),
    };

    // 1. 패턴 단독 검증 (이스케이프 본문 제외)
    if !opts.word_match {
        return compile(&body).map(|regex| adapt(regex, &body));
    }
    if !escaped {
        compile(&body)?;
    }

    // 2. rg -w 래핑 ((?x) 주석 개행 종료)
    let wrapped = format!(r"\b(?:{body})\b");
    match compile(&wrapped) {
        Err(_) if body.contains('#') => {
            let closed = format!("\\b(?:{body}\n)\\b");
            compile(&closed).map(|regex| adapt(regex, &closed))
        }
        compiled => compiled.map(|regex| adapt(regex, &wrapped)),
    }
}
// 버퍼 단위 탐색 적격 판정
fn suits_buffer_search(source: &str, opts: &PatternOpts) -> bool {
    // 1. 컴파일 동일 플래그 HIR
    let parsed = regex_syntax::ParserBuilder::new()
        .utf8(false)
        .case_insensitive(opts.ignore_case)
        .multi_line(true)
        .crlf(true)
        .dot_matches_new_line(opts.multiline)
        .build()
        .parse(source);
    let Ok(hir) = parsed else {
        return false;
    };

    // 2. LF·CR 소비와 줄 밖 문맥 의존 탐지
    let mut pending = vec![&hir];
    while let Some(node) = pending.pop() {
        let line_bound = match node.kind() {
            HirKind::Empty => true,
            HirKind::Literal(literal) => !literal.0.contains(&b'\n') && !literal.0.contains(&b'\r'),
            HirKind::Class(Class::Unicode(class)) => !class.ranges().iter().any(|range| {
                let span = range.start()..=range.end();
                span.contains(&'\n') || span.contains(&'\r')
            }),
            HirKind::Class(Class::Bytes(class)) => !class.ranges().iter().any(|range| {
                let span = range.start()..=range.end();
                span.contains(&b'\n') || span.contains(&b'\r')
            }),
            // CRLF 줄 앵커·양쪽 단어 경계만 동치
            HirKind::Look(look) => matches!(
                look,
                Look::StartCRLF
                    | Look::EndCRLF
                    | Look::WordAscii
                    | Look::WordUnicode
                    | Look::WordStartAscii
                    | Look::WordEndAscii
                    | Look::WordStartUnicode
                    | Look::WordEndUnicode
            ),
            HirKind::Repetition(repetition) => {
                pending.push(&repetition.sub);
                true
            }
            HirKind::Capture(capture) => {
                pending.push(&capture.sub);
                true
            }
            HirKind::Concat(parts) | HirKind::Alternation(parts) => {
                pending.extend(parts);
                true
            }
        };
        if !line_bound {
            return false;
        }
    }
    if !hir.properties().look_set().contains_word_unicode() {
        return true;
    }

    // 3. 유니코드 단어 경계의 접두 리터럴 요구 (비ASCII DFA 이탈)
    let prefixes = regex_syntax::hir::literal::Extractor::new().extract(&hir);
    prefixes.literals().is_some_and(|literals| {
        !literals.is_empty() && literals.iter().all(|literal| !literal.is_empty())
    })
}
// backtracking 엔진(fancy-regex) 컴파일: lookaround/backreference 지원. 옵션은 빌더 세터
// (인라인 접두 없음, 오류 위치 = 사용자 패턴 위치). 폭주는 격리 워커 kill이 상한.
pub(super) fn build_fancy_matcher(
    pattern: &str,
    opts: &PatternOpts,
) -> Result<FancyAdapter, fancy_regex::Error> {
    let compile = |source: &str| {
        fancy_regex::RegexBuilder::new(source)
            .multi_line(true)
            .crlf(true)
            .case_insensitive(opts.ignore_case)
            .dot_matches_new_line(opts.multiline)
            .backtrack_limit(FANCY_BACKTRACK_LIMIT)
            .build()
    };

    // 1. 패턴 단독 검증
    let trims_cr = !opts.multiline;
    let alone = compile(pattern)?;
    if !opts.word_match {
        return Ok(FancyAdapter {
            regex: alone,
            trims_cr,
        });
    }

    // 2. rg -w 래핑 ((?x) 주석 개행 종료)
    let regex = match compile(&format!(r"\b(?:{pattern})\b")) {
        Err(_) if pattern.contains('#') => compile(&format!("\\b(?:{pattern}\n)\\b")),
        wrapped => wrapped,
    }?;
    Ok(FancyAdapter { regex, trims_cr })
}
// linear engine(Rust regex)이 문법으로는 인지하지만 실행하지 못하는 구문 판별. 이때의 리터럴
// 폴백은 패턴 텍스트 자체를 찾는 오검색이라 대신 fancy 라우팅 또는 크기 초과 거절로 분기한다.
pub(super) enum Unsupported {
    Fancy,
    TooBig(usize),
}
pub(super) fn classify_unsupported(error: &regex::Error) -> Option<Unsupported> {
    if let regex::Error::CompiledTooBig(limit) = error {
        return Some(Unsupported::TooBig(*limit));
    }
    let text = error.to_string();
    if text.contains("look-around") || text.contains("backreference") {
        return Some(Unsupported::Fancy);
    }
    None
}
// fancy 패턴 자체 오류 판별 (크기 초과·미지원·런타임 제외)
pub(super) fn is_fancy_syntax_error(error: &fancy_regex::Error) -> bool {
    use fancy_regex::{CompileError, Error, ParseError};
    match error {
        // 엔진 한계 (중첩 64단·인라인 u 전환)
        Error::ParseError(
            _,
            ParseError::RecursionExceeded | ParseError::ChangingUnicodeModeUnsupported,
        ) => false,
        Error::ParseError(..) => true,
        Error::CompileError(compile) => match compile.as_ref() {
            // 위임 regex 문법 오류만 (크기 상한 제외)
            CompileError::InnerError(inner) => inner.syntax_error().is_some(),
            CompileError::InvalidGroupName
            | CompileError::InvalidGroupNameBackref(_)
            | CompileError::InvalidBackref(_)
            | CompileError::NamedBackrefOnly
            | CompileError::SubroutineCallTargetNotFound(..)
            | CompileError::LeftRecursiveSubroutineCall(_)
            | CompileError::NeverEndingRecursion => true,
            _ => false,
        },
        _ => false,
    }
}
// regex 파스 오류는 캐럿 포함 다중 줄이라 마지막 "error: ..." 요지만 라벨에 싣는다.
pub(super) fn parse_error_gist(error: &regex::Error) -> String {
    let text = error.to_string();
    text.lines()
        .rev()
        .find_map(|line| line.strip_prefix("error: "))
        .unwrap_or("regex parse error")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // find_at 호출 계수 래퍼
    struct CountingMatcher {
        adapter: RegexAdapter,
        calls: std::cell::Cell<usize>,
    }
    impl Matcher for CountingMatcher {
        type Captures = NoCaptures;
        type Error = NoError;
        fn find_at(&self, haystack: &[u8], at: usize) -> Result<Option<Match>, NoError> {
            self.calls.set(self.calls.get() + 1);
            self.adapter.find_at(haystack, at)
        }
        fn new_captures(&self) -> Result<NoCaptures, NoError> {
            Ok(NoCaptures::new())
        }
        fn line_terminator(&self) -> Option<LineTerminator> {
            self.adapter.line_terminator()
        }
    }

    #[test]
    fn line_bound_matcher_searches_each_buffer_once() {
        let opts = PatternOpts {
            ignore_case: false,
            literal: false,
            multiline: false,
            word_match: false,
        };
        let haystack = "hay line\r\n".repeat(500);
        for (line_bound, calls) in [(true, 1), (false, 500)] {
            let mut adapter = build_matcher("needle", &opts, false).unwrap();
            assert!(adapter.line_bound);
            adapter.line_bound = line_bound;
            let counting = CountingMatcher {
                adapter,
                calls: std::cell::Cell::new(0),
            };
            grep_searcher::Searcher::new()
                .search_slice(
                    &counting,
                    haystack.as_bytes(),
                    grep_searcher::sinks::UTF8(|_, _| Ok(true)),
                )
                .unwrap();
            assert_eq!(counting.calls.get(), calls, "{line_bound}");
        }
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
    fn fancy_engine_limits_are_errors_not_literal_fallback() {
        let dir =
            std::env::temp_dir().join(format!("rust-fs-mcp-fancy-limits-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "abc\n").unwrap();
        let nested = format!("{}a{}", "(?=".repeat(70), ")".repeat(70));
        for (pattern, reason) in [
            (r"(?-u:\xFF)(?=a)".to_string(), "Unicode mode"),
            (nested, "too deeply nested"),
        ] {
            let response = crate::tools::dispatch_tool_call(
                "fs-search",
                Some(serde_json::json!({ "items": [{
                    "path": dir.display().to_string(),
                    "pattern": pattern
                }] })),
            );
            let text = response.to_string();
            assert!(
                text.contains("Invalid search pattern (backtracking engine)"),
                "{pattern}: {text}"
            );
            assert!(text.contains(reason), "{pattern}: {text}");
            assert!(!text.contains("literal fallback"), "{pattern}: {text}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
