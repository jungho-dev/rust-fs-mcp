//! matcher.rs
//! tools::search::matcher
//!
//! Linear (regex) and backtracking (fancy-regex) grep matchers plus smart-case pattern analysis.
//!

use grep_matcher::{Match, Matcher, NoCaptures, NoError};

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
// 3. Pattern compilation ------------------------------------------------------
// 패턴 컴파일 옵션: literal=rg -F, word_match=rg -w, multiline=rg -U(+dot-all) 대응.
pub(super) struct PatternOpts {
    pub(super) ignore_case: bool,
    pub(super) literal: bool,
    pub(super) multiline: bool,
    pub(super) word_match: bool,
}
pub(super) fn build_matcher(
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
pub(super) fn build_fancy_matcher(
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
}
