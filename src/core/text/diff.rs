//! diff.rs
//! core::text::diff
//!
//! Shared unified-diff engine (similar) behind file-diff, dryRun previews, file-replace, and ast-search rewrites.
//! CRLF is folded to LF before diffing so an EOL-only difference never floods every hunk; eolChanged reports it instead.
//! The context radius is clamped to MAX_DIFF_CONTEXT so similar's hunk grouping cannot overflow.
//!

use similar::{Algorithm, DiffTag, TextDiff, WhitespaceMode};
use std::borrow::Cow;
use std::time::Duration;

// 문맥 줄 상한 (similar n*2 overflow 방지)
pub const MAX_DIFF_CONTEXT: usize = 10_000;

// 1. Diff spec ----------------------------------------------------------------
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DiffAlgorithm {
    #[default]
    Myers,
    Patience,
    Histogram,
}
impl DiffAlgorithm {
    pub fn parse_name(name: &str) -> Result<Self, String> {
        match name.to_ascii_lowercase().as_str() {
            "myers" => Ok(Self::Myers),
            "patience" => Ok(Self::Patience),
            "histogram" => Ok(Self::Histogram),
            other => Err(format!(
                "Unknown diff algorithm '{other}'; use myers, patience, or histogram"
            )),
        }
    }
    fn engine(self) -> Algorithm {
        match self {
            Self::Myers => Algorithm::Myers,
            Self::Patience => Algorithm::Patience,
            Self::Histogram => Algorithm::Histogram,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct DiffSpec {
    pub context: usize,
    pub algorithm: DiffAlgorithm,
    pub ignore_whitespace: bool,
    pub timeout: Duration,
}
impl Default for DiffSpec {
    fn default() -> Self {
        Self {
            context: 3,
            algorithm: DiffAlgorithm::Myers,
            ignore_whitespace: false,
            // 대형 입력 상한 (초과 시 근사 diff)
            timeout: Duration::from_millis(500),
        }
    }
}
#[derive(Debug, Default)]
pub struct DiffSummary {
    pub unified: String,
    pub insertions: usize,
    pub deletions: usize,
    pub hunks: usize,
    pub identical: bool,
    pub eol_changed: bool,
}

// 2. Unified diff -------------------------------------------------------------
pub fn render_unified_diff(
    old: &str,
    new: &str,
    old_label: &str,
    new_label: &str,
    spec: &DiffSpec,
) -> DiffSummary {
    let old_text = fold_crlf(old);
    let new_text = fold_crlf(new);
    // 종결자 양식 차이, 또는 접기 후 동일한 원본 차이
    let eol_changed = match (line_ending_style(old), line_ending_style(new)) {
        (Some(left), Some(right)) => left != right,
        _ => false,
    } || (old != new && old_text == new_text);
    let context = spec.context.min(MAX_DIFF_CONTEXT);
    let mode = if spec.ignore_whitespace {
        WhitespaceMode::IgnoreChanges
    } else {
        WhitespaceMode::Exact
    };
    let diff = TextDiff::configure()
        .algorithm(spec.algorithm.engine())
        .timeout(spec.timeout)
        .whitespace_mode(mode)
        .diff_lines(old_text.as_ref(), new_text.as_ref());

    // 줄 단위 증감 집계
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    for op in diff.ops() {
        match op.tag() {
            DiffTag::Equal => {}
            DiffTag::Delete => deletions += op.old_range().len(),
            DiffTag::Insert => insertions += op.new_range().len(),
            DiffTag::Replace => {
                deletions += op.old_range().len();
                insertions += op.new_range().len();
            }
        }
    }
    let identical = insertions == 0 && deletions == 0;
    if identical {
        return DiffSummary {
            identical,
            eol_changed,
            ..DiffSummary::default()
        };
    }
    let hunks = diff
        .grouped_ops(context)
        .iter()
        .filter(|group| !group.is_empty())
        .count();
    let unified = diff
        .unified_diff()
        .context_radius(context)
        .header(old_label, new_label)
        .missing_newline_hint(true)
        .to_string();
    DiffSummary {
        unified,
        insertions,
        deletions,
        hunks,
        identical,
        eol_changed,
    }
}
// 줄 종결자 양식 (종결자 없으면 None)
pub fn line_ending_style(text: &str) -> Option<&'static str> {
    let bytes = text.as_bytes();
    let has_crlf = text.contains("\r\n");
    let has_bare_lf = memchr::memchr_iter(b'\n', bytes)
        .any(|at| at.checked_sub(1).and_then(|before| bytes.get(before)) != Some(&b'\r'));
    match (has_crlf, has_bare_lf) {
        (true, true) => Some("mixed"),
        (true, false) => Some("CRLF"),
        (false, true) => Some("LF"),
        (false, false) => None,
    }
}
fn fold_crlf(text: &str) -> Cow<'_, str> {
    if text.contains("\r\n") {
        Cow::Owned(text.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(text)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_git_style_hunks_with_counts() {
        let summary = render_unified_diff(
            "a\nb\nc\n",
            "a\nB\nc\nd\n",
            "a/x.txt",
            "b/x.txt",
            &DiffSpec::default(),
        );
        assert!(!summary.identical);
        assert_eq!((summary.insertions, summary.deletions), (2, 1));
        assert_eq!(summary.hunks, 1);
        assert!(summary.unified.starts_with("--- a/x.txt\n+++ b/x.txt\n@@ "));
        assert!(summary.unified.contains("-b\n+B\n"), "{}", summary.unified);
    }
    #[test]
    fn eol_only_change_is_identical_but_flagged() {
        let summary = render_unified_diff("a\r\nb\r\n", "a\nb\n", "a", "b", &DiffSpec::default());
        assert!(summary.identical);
        assert!(summary.eol_changed);
        assert!(summary.unified.is_empty());
    }
    #[test]
    fn crlf_edit_shows_only_the_changed_line() {
        let summary = render_unified_diff(
            "one\r\ntwo\r\nthree\r\n",
            "one\r\nTWO\r\nthree\r\n",
            "a",
            "b",
            &DiffSpec::default(),
        );
        assert_eq!((summary.insertions, summary.deletions), (1, 1));
        assert!(!summary.unified.contains('\r'));
        assert!(!summary.eol_changed);
    }
    #[test]
    fn missing_trailing_newline_is_hinted() {
        let summary = render_unified_diff("a\n", "a\nb", "a", "b", &DiffSpec::default());
        assert!(summary.unified.contains("\\ No newline at end of file"));
    }
    #[test]
    fn whitespace_mode_ignores_indent_changes() {
        let spec = DiffSpec {
            ignore_whitespace: true,
            ..DiffSpec::default()
        };
        let summary = render_unified_diff("  x\n", "    x\n", "a", "b", &spec);
        assert!(summary.identical);
    }
    #[test]
    fn huge_context_is_clamped_instead_of_overflowing() {
        let expected =
            render_unified_diff("a\nb\nc\n", "a\nB\nc\n", "a", "b", &DiffSpec::default());
        for context in [usize::MAX, 1usize << 63] {
            let spec = DiffSpec {
                context,
                ..DiffSpec::default()
            };
            let summary = render_unified_diff("a\nb\nc\n", "a\nB\nc\n", "a", "b", &spec);
            assert_eq!(summary.unified, expected.unified, "context {context}");
            assert_eq!(summary.hunks, 1);
        }
    }
    #[test]
    fn mixed_line_endings_are_flagged_against_uniform_ones() {
        let spec = DiffSpec::default();
        let mixed_vs_crlf = render_unified_diff("a\r\nb\n", "a\r\nb\r\n", "a", "b", &spec);
        assert!(mixed_vs_crlf.identical);
        assert!(mixed_vs_crlf.eol_changed);
        let mixed_vs_lf = render_unified_diff("a\r\nb\n", "a\nb\n", "a", "b", &spec);
        assert!(mixed_vs_lf.identical);
        assert!(mixed_vs_lf.eol_changed);
        let moved_crlf = render_unified_diff("a\r\nb\n", "a\nb\r\n", "a", "b", &spec);
        assert!(moved_crlf.identical);
        assert!(moved_crlf.eol_changed);
        let edited_mixed = render_unified_diff("a\r\nb\n", "a\r\nB\r\n", "a", "b", &spec);
        assert!(!edited_mixed.identical);
        assert!(edited_mixed.eol_changed);
    }
    #[test]
    fn same_line_endings_are_not_flagged() {
        let spec = DiffSpec::default();
        let same_bytes = render_unified_diff("a\r\nb\n", "a\r\nb\n", "a", "b", &spec);
        assert!(same_bytes.identical);
        assert!(!same_bytes.eol_changed);
        let lf_edit = render_unified_diff("a\nb\n", "a\nB\n", "a", "b", &spec);
        assert!(!lf_edit.identical);
        assert!(!lf_edit.eol_changed);
        let grown_line = render_unified_diff("a", "a\nb", "a", "b", &spec);
        assert!(!grown_line.eol_changed);
    }
    #[test]
    fn parses_algorithm_names() {
        assert_eq!(
            DiffAlgorithm::parse_name("Histogram").unwrap(),
            DiffAlgorithm::Histogram
        );
        assert!(DiffAlgorithm::parse_name("lcs").is_err());
    }
}
