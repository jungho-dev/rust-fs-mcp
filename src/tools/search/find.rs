//! find.rs
//! tools::search::find
//!
//! path-find handler: fd-style name, type, extension, size, and mtime filters over the ignore walker.
//! Each walk thread keeps a bounded top set in sort order (maxResults and output budget) and the sets
//! merge after the walk, so a capped listing is the exact first page, not the first entries found.
//! Offset-free dates resolve in the local zone like fd: a DST fold takes the later instant, a gap
//! keeps the pre-transition offset (02:30 in a 02:00-03:00 gap is 03:30).
//!

use super::matcher::has_uppercase_literal;
use super::walk::{ignore_walk_builder, search_threads};
use crate::core::batch::{SEARCH_PLAN, create_batch_response, run_batch_parallel};
use crate::core::config::ensure_path_allowed;
use crate::core::response::{RawResult, max_output_bytes};
use crate::tools::common::{bool_field, path_in_heavy_dir};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkState;
use ignore::overrides::OverrideBuilder;
use regex::bytes::{Regex, RegexBuilder};
use serde_json::{Value, json};
use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FIND_BACKEND: &str = "native-walk";
// fs-search 동일 기본 시간 예산
const FIND_TIMEOUT_MS: u64 = 25_000;

// 1. Find spec and entries ----------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Any,
    File,
    Directory,
    Symlink,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Path,
    Modified,
    Size,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SizeBound {
    AtLeast(u64),
    AtMost(u64),
    Exactly(u64),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimeBound {
    After(SystemTime),
    Before(SystemTime),
}
struct FindSpec {
    root: PathBuf,
    name_regex: Option<Regex>,
    name_globs: Option<GlobSet>,
    full_path: bool,
    kind: EntryKind,
    empty_only: bool,
    extensions: Vec<String>,
    sizes: Vec<SizeBound>,
    times: Vec<TimeBound>,
    min_depth: usize,
    max_depth: Option<usize>,
    include_hidden: bool,
    respect_ignore: bool,
    default_excludes: bool,
    exclude_globs: Vec<String>,
    follow_links: bool,
    sort: SortKey,
    details: bool,
    max_results: usize,
    byte_budget: usize,
    timeout_ms: u64,
    local_offsets: Vec<i64>,
}
// 정렬 순위·상대 경로·표시 꼬리
#[derive(PartialEq, Eq)]
struct FoundEntry {
    rank: i128,
    rel: String,
    tail: String,
}
impl Ord for FoundEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        // 컴포넌트 단위 바이트 순서 (a < a/b < a-x)
        let low = |byte: u8| if byte == b'/' { 0 } else { byte };
        self.rank
            .cmp(&other.rank)
            .then_with(|| self.rel.bytes().map(low).cmp(other.rel.bytes().map(low)))
    }
}
impl PartialOrd for FoundEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
// 정렬 앞 페이지 집합 (heap 최상단 최후순)
#[derive(Default)]
struct TopSet {
    heap: BinaryHeap<FoundEntry>,
    bytes: usize,
    floor: Option<FoundEntry>,
    clipped: bool,
}
// 워커 종료 시 로컬 집합 반납
struct WorkerSink<'a> {
    set: TopSet,
    shared: &'a Mutex<Vec<TopSet>>,
}
impl Drop for WorkerSink<'_> {
    fn drop(&mut self) {
        if self.set.heap.is_empty() && !self.set.clipped {
            return;
        }
        let set = std::mem::take(&mut self.set);
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(set);
    }
}
struct FindOutcome {
    entries: Vec<FoundEntry>,
    timed_out: bool,
    partial: bool,
    limit_reached: bool,
}

// 2. Path find tool -----------------------------------------------------------
pub fn handle_path_find(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };
    let results = run_batch_parallel(items, SEARCH_PLAN, find_item);
    create_batch_response("path-find", results, true)
}
fn find_item(item: &Value) -> RawResult {
    let spec = match read_find_spec(item) {
        Ok(spec) => spec,
        Err(error) => return RawResult::error(error),
    };
    let outcome = match run_find_walk(&spec) {
        Ok(outcome) => outcome,
        Err(error) => return RawResult::error(error),
    };
    // 수집분 없는 타임아웃 오류
    if outcome.timed_out && outcome.entries.is_empty() {
        return RawResult::error(format!(
            "path-find timed out after {}ms with no entries yet; narrow the path or the filters, or raise timeout_ms",
            spec.timeout_ms
        ));
    }

    // 루트 줄, 상태 표식, 항목 순서
    let root_label = spec.root.display().to_string();
    let listed_bytes: usize = outcome
        .entries
        .iter()
        .map(|found| found.rel.len() + found.tail.len() + 1)
        .sum();
    let mut text = String::with_capacity(root_label.len() + listed_bytes + 256);
    text.push_str(&root_label);
    text.push(':');
    if outcome.timed_out {
        text.push_str("\n[walk stopped at the time budget; results are partial. Narrow the path or raise timeout_ms]");
    }
    if outcome.limit_reached {
        text.push_str("\n[stopped at maxResults or the output budget; more entries match. Narrow with pattern, glob, or a deeper path]");
    }
    if outcome.partial {
        text.push_str("\n[some entries could not be read; the listing may be incomplete]");
    }
    if !spec.local_offsets.is_empty() {
        // 적용 오프셋 표기 (LMT 초 단위 포함)
        let labels: Vec<String> = spec
            .local_offsets
            .iter()
            .map(|&offset| {
                let sign = if offset < 0 { '-' } else { '+' };
                let magnitude = offset.unsigned_abs();
                let (hours, minutes) = (magnitude / 3_600, magnitude / 60 % 60);
                match magnitude % 60 {
                    0 => format!("UTC{sign}{hours:02}:{minutes:02}"),
                    seconds => format!("UTC{sign}{hours:02}:{minutes:02}:{seconds:02}"),
                }
            })
            .collect();
        text.push_str(&format!(
            "\n[a date without an offset was read as local time ({}), like fd; append Z or +HH:MM to be explicit]",
            labels.join(", ")
        ));
    }
    if outcome.entries.is_empty() {
        text.push_str("\n(no matches)");
    }
    for found in &outcome.entries {
        text.push('\n');
        text.push_str(&found.rel);
        text.push_str(&found.tail);
    }

    let mut structured = json!({
        "path": root_label,
        "backend": FIND_BACKEND,
        "totalCount": outcome.entries.len(),
    });
    for (key, flag) in [
        ("limitReached", outcome.limit_reached),
        ("timedOut", outcome.timed_out),
        ("partial", outcome.partial),
    ] {
        if flag {
            structured[key] = json!(true);
        }
    }
    let mut result = RawResult::structured(text, structured);
    result
        .meta
        .insert("backend".to_string(), json!(FIND_BACKEND));
    result
}

// 3. Argument readers ---------------------------------------------------------
fn read_find_spec(item: &Value) -> Result<FindSpec, String> {
    // 1. 루트 경로
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return Err("path must be a string".to_string());
    };
    let root = ensure_path_allowed(path)?;
    let Ok(root_meta) = std::fs::metadata(&root) else {
        return Err(format!("Path does not exist: {}", root.display()));
    };
    if !root_meta.is_dir() {
        return Err(format!(
            "path-find needs a directory root; {} is a file (use path-stat for one path's metadata)",
            root.display()
        ));
    }

    // 2. 이름 regex·glob (smart-case)
    let full_path = bool_field(item, "fullPath", false);
    let ignore_case = item.get("ignoreCase").and_then(Value::as_bool);
    let pattern = item.get("pattern").and_then(Value::as_str);
    let name_regex = match pattern {
        Some(pattern) => Some(
            RegexBuilder::new(pattern)
                .case_insensitive(
                    ignore_case.unwrap_or_else(|| !has_uppercase_literal(pattern, false)),
                )
                .build()
                .map_err(|error| format!("Invalid pattern regex: {error}"))?,
        ),
        None => None,
    };
    let glob_alternatives: Vec<&str> = item
        .get("glob")
        .and_then(Value::as_str)
        .into_iter()
        .flat_map(|glob| glob.split('|'))
        .map(str::trim)
        .filter(|glob| !glob.is_empty())
        .collect();
    let name_globs = if glob_alternatives.is_empty() {
        None
    } else {
        let mut globs = GlobSetBuilder::new();
        for glob in &glob_alternatives {
            let compiled = GlobBuilder::new(glob)
                .case_insensitive(ignore_case.unwrap_or_else(|| !has_uppercase_literal(glob, true)))
                .literal_separator(full_path)
                .build()
                .map_err(|error| format!("Invalid glob '{glob}': {error}"))?;
            globs.add(compiled);
        }
        Some(globs.build().map_err(|error| error.to_string())?)
    };
    // 점 시작 패턴 hidden 자동 포함 (명시값 우선)
    let dot_pattern = pattern.is_some_and(|pattern| pattern.starts_with("^\\."));
    let dot_glob = glob_alternatives.iter().any(|glob| {
        glob.split('/')
            .any(|segment| segment.starts_with('.') && segment != "." && segment != "..")
    });
    let include_hidden = item
        .get("includeHidden")
        .and_then(Value::as_bool)
        .unwrap_or(dot_pattern || dot_glob);

    // 3. 종류·확장자·정렬
    let kind = match item.get("type").and_then(Value::as_str).unwrap_or("any") {
        "any" => EntryKind::Any,
        "file" | "f" => EntryKind::File,
        "directory" | "dir" | "d" => EntryKind::Directory,
        "symlink" | "l" => EntryKind::Symlink,
        other => {
            return Err(format!(
                "type '{other}' is not supported; use any, file, directory, or symlink"
            ));
        }
    };
    let extensions: Vec<String> = item
        .get("extension")
        .and_then(Value::as_str)
        .into_iter()
        .flat_map(|text| text.split(['|', ',']))
        .map(|ext| ext.trim().trim_start_matches('.'))
        .filter(|ext| !ext.is_empty())
        .map(|ext| format!(".{}", ext.to_lowercase()))
        .collect();
    if item.get("extension").is_some() && extensions.is_empty() {
        return Err("extension must name at least one extension, e.g. rs or rs|toml".to_string());
    }
    let sort = match item.get("sortBy").and_then(Value::as_str).unwrap_or("path") {
        "path" => SortKey::Path,
        "modified" => SortKey::Modified,
        "size" => SortKey::Size,
        other => {
            return Err(format!(
                "sortBy '{other}' is not supported; use path, modified, or size"
            ));
        }
    };

    // 4. 크기·시간 필터
    let sizes = match item.get("size").and_then(Value::as_str) {
        Some(text) => parse_size_bounds(text)?,
        None => Vec::new(),
    };
    let mut times = Vec::new();
    let mut local_offsets = Vec::new();
    for field in ["changedWithin", "changedBefore"] {
        let Some(text) = item.get(field).and_then(Value::as_str) else {
            continue;
        };
        let (point, local_offset) = parse_time_point(field, text)?;
        if let Some(offset) = local_offset
            && !local_offsets.contains(&offset)
        {
            local_offsets.push(offset);
        }
        times.push(if field == "changedWithin" {
            TimeBound::After(point)
        } else {
            TimeBound::Before(point)
        });
    }

    // 5. 제외 glob·예산
    let mut exclude_globs = Vec::new();
    if let Some(patterns) = item.get("excludePatterns") {
        let Some(patterns) = patterns.as_array() else {
            return Err("excludePatterns must be an array of glob strings".to_string());
        };
        for pattern in patterns {
            let Some(pattern) = pattern.as_str() else {
                return Err("excludePatterns must be an array of glob strings".to_string());
            };
            exclude_globs.push(pattern.to_string());
        }
    }
    let read_usize = |key: &str| {
        item.get(key)
            .and_then(Value::as_u64)
            .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
    };
    Ok(FindSpec {
        name_regex,
        name_globs,
        full_path,
        kind,
        empty_only: bool_field(item, "empty", false),
        extensions,
        sizes,
        times,
        min_depth: read_usize("minDepth").unwrap_or(0),
        max_depth: read_usize("maxDepth"),
        include_hidden,
        respect_ignore: bool_field(item, "respectGitignore", true),
        // heavy dir 내부 루트는 기본 제외 해제
        default_excludes: !bool_field(item, "noDefaultExcludes", false)
            && !path_in_heavy_dir(&root),
        exclude_globs,
        follow_links: bool_field(item, "followLinks", false),
        sort,
        details: bool_field(item, "details", false),
        max_results: read_usize("maxResults").unwrap_or(usize::MAX),
        // 응답 예산 80% 수집 (이스케이프 여유)
        byte_budget: max_output_bytes() * 4 / 5,
        timeout_ms: item
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(FIND_TIMEOUT_MS),
        local_offsets,
        root,
    })
}
// fd -S 크기 문법
fn parse_size_bounds(text: &str) -> Result<Vec<SizeBound>, String> {
    let invalid = || {
        format!(
            "Invalid size '{text}': use +N (at least), -N (at most), or N (exactly) with a unit: b, k/kb (1000), ki/kib (1024), m/mb, mi/mib, g/gb, gi/gib, t/tb, ti/tib; e.g. +10k, -1mi, 500b; join bounds with a comma (+1k,-1m)"
        )
    };
    let mut bounds = Vec::new();
    for piece in text.split(',') {
        let piece = piece.trim().to_ascii_lowercase();
        let (sign, body) = if let Some(body) = piece.strip_prefix('+') {
            ('+', body)
        } else if let Some(body) = piece.strip_prefix('-') {
            ('-', body)
        } else {
            ('=', piece.as_str())
        };
        let digits_end = body
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(body.len());
        let (digits, unit) = body.split_at(digits_end);
        let amount: u64 = digits.parse().map_err(|_| invalid())?;
        let scale: u64 = match unit {
            "b" => 1,
            "k" | "kb" => 1_000,
            "ki" | "kib" => 1 << 10,
            "m" | "mb" => 1_000_000,
            "mi" | "mib" => 1 << 20,
            "g" | "gb" => 1_000_000_000,
            "gi" | "gib" => 1 << 30,
            "t" | "tb" => 1_000_000_000_000,
            "ti" | "tib" => 1 << 40,
            _ => return Err(invalid()),
        };
        let bytes = amount.checked_mul(scale).ok_or_else(invalid)?;
        bounds.push(match sign {
            '+' => SizeBound::AtLeast(bytes),
            '-' => SizeBound::AtMost(bytes),
            _ => SizeBound::Exactly(bytes),
        });
    }
    Ok(bounds)
}
// 기간·절대 날짜 시점 해석 (로컬 해석 시 오프셋 반환)
fn parse_time_point(field: &str, text: &str) -> Result<(SystemTime, Option<i64>), String> {
    let invalid = || {
        format!(
            "Invalid {field} '{text}': use a duration (30s, 10m, 2h, 7d, 2w; long forms such as 30min, 2hours, 1day, 2weeks; months and years are not accepted) or a date (2026-11-01, \"2026-11-01 13:30:00\", 2026-11-01T13:30:00Z, 2026-11-01T13:30:00+09:00, @1700000000); dates without an offset are read as local time (the TZ variable, else the system time zone), like fd"
        )
    };
    let trimmed = text.trim();
    let from_epoch = |seconds: i64| {
        if seconds >= 0 {
            UNIX_EPOCH.checked_add(Duration::from_secs(seconds.unsigned_abs()))
        } else {
            UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
        }
    };

    // 1. @epoch 초
    if let Some(digits) = trimmed.strip_prefix('@') {
        let seconds: i64 = digits.parse().map_err(|_| invalid())?;
        return from_epoch(seconds)
            .map(|point| (point, None))
            .ok_or_else(invalid);
    }

    // 2. 절대 날짜 (오프셋 없으면 로컬 시간대)
    if trimmed.as_bytes().get(4) == Some(&b'-') {
        let (seconds, naive) = parse_civil_seconds(trimmed).ok_or_else(invalid)?;
        let (seconds, local_offset) = if naive {
            let (utc, offset) = local_zone::local_to_utc_seconds(seconds).map_err(|reason| {
                format!("Invalid {field} '{text}': {reason}; append Z or +HH:MM")
            })?;
            (utc, Some(offset))
        } else {
            (seconds, None)
        };
        return from_epoch(seconds)
            .map(|point| (point, local_offset))
            .ok_or_else(invalid);
    }

    // 3. 기간 (숫자+단위 반복, 예: 1h30m)
    let mut total: u64 = 0;
    let mut rest = trimmed;
    if rest.is_empty() {
        return Err(invalid());
    }
    while !rest.is_empty() {
        let digits_end = rest
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(rest.len());
        let (digits, after) = rest.split_at(digits_end);
        let amount: u64 = digits.parse().map_err(|_| invalid())?;
        let after = after.trim_start();
        let unit_end = after
            .find(|ch: char| !ch.is_ascii_alphabetic())
            .unwrap_or(after.len());
        let (unit, after) = after.split_at(unit_end);
        let scale: u64 = match unit.to_ascii_lowercase().as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
            "d" | "day" | "days" => 86_400,
            "w" | "week" | "weeks" => 604_800,
            _ => return Err(invalid()),
        };
        total = amount
            .checked_mul(scale)
            .and_then(|seconds| total.checked_add(seconds))
            .ok_or_else(invalid)?;
        rest = after.trim_start();
    }
    SystemTime::now()
        .checked_sub(Duration::from_secs(total))
        .map(|point| (point, None))
        .ok_or_else(invalid)
}
// RFC 3339 부분집합 → epoch 초
fn parse_civil_seconds(text: &str) -> Option<(i64, bool)> {
    let bytes = text.as_bytes();
    let number = |start: usize, len: usize| -> Option<i64> {
        let digits = bytes.get(start..start + len)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(digits).ok()?.parse().ok()
    };

    // 1. 날짜 (윤년 검증)
    if bytes.get(7) != Some(&b'-') {
        return None;
    }
    let (year, month, day) = (number(0, 4)?, number(5, 2)?, number(8, 2)?);
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let max_day = *month_days.get(usize::try_from(month - 1).ok()?)?;
    if !(1..=max_day).contains(&day) {
        return None;
    }
    let mut seconds = days_from_civil(year, month, day) * 86_400;
    if bytes.len() == 10 {
        return Some((seconds, true));
    }

    // 2. 시각
    if !matches!(bytes.get(10), Some(b'T' | b't' | b' ')) || bytes.get(13) != Some(&b':') {
        return None;
    }
    let (hour, minute) = (number(11, 2)?, number(14, 2)?);
    let mut second = 0;
    let mut cursor = 16;
    if bytes.get(cursor) == Some(&b':') {
        second = number(17, 2)?;
        cursor = 19;
        if bytes.get(cursor) == Some(&b'.') {
            cursor += 1;
            let frac_start = cursor;
            while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
                cursor += 1;
            }
            if cursor == frac_start {
                return None;
            }
        }
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    seconds += hour * 3_600 + minute * 60 + second;

    // 3. 오프셋 (Z, ±HH:MM, ±HHMM)
    let offset = bytes.get(cursor..)?;
    let offset_seconds = match offset.first() {
        None => return Some((seconds, true)),
        Some(b'Z' | b'z') if offset.len() == 1 => 0,
        Some(sign @ (b'+' | b'-')) => {
            let minute_start = if offset.get(3) == Some(&b':') { 4 } else { 3 };
            if offset.len() != minute_start + 2 {
                return None;
            }
            let hours = number(cursor + 1, 2)?;
            let minutes = number(cursor + minute_start, 2)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let magnitude = hours * 3_600 + minutes * 60;
            if *sign == b'+' { magnitude } else { -magnitude }
        }
        Some(_) => return None,
    };
    Some((seconds - offset_seconds, false))
}

// 4. Parallel walk and filters ------------------------------------------------
fn run_find_walk(spec: &FindSpec) -> Result<FindOutcome, String> {
    // 1. fd --exclude 동일 overrides
    let mut overrides = OverrideBuilder::new(&spec.root);
    for glob in &spec.exclude_globs {
        overrides
            .add(&format!("!{}", glob.trim_start_matches('!')))
            .map_err(|error| format!("Invalid excludePatterns glob '{glob}': {error}"))?;
    }
    let overrides = overrides.build().map_err(|error| error.to_string())?;
    // 걷지 않을 디렉터리는 규모 추정 제외
    let threads = search_threads(
        &spec.root,
        Some(|name: &OsStr| {
            let hidden = name.as_encoded_bytes().first() == Some(&b'.');
            (hidden && !spec.include_hidden)
                || (spec.default_excludes
                    && (name == "node_modules" || name == "target" || name == ".git"))
        }),
    );
    let mut builder = ignore_walk_builder(&spec.root, spec.include_hidden, spec.respect_ignore);
    builder
        .overrides(overrides)
        .follow_links(spec.follow_links)
        .max_depth(spec.max_depth)
        .threads(threads);

    // 2. 워커별 필터·상위 집합
    // 표현 불가 기한은 무기한
    let deadline = Instant::now().checked_add(Duration::from_millis(spec.timeout_ms));
    let shared = Mutex::new(Vec::new());
    let timed_out = AtomicBool::new(false);
    let partial = AtomicBool::new(false);
    let needs_metadata = !spec.sizes.is_empty()
        || !spec.times.is_empty()
        || spec.details
        || spec.empty_only
        || spec.sort != SortKey::Path;
    let build_rel = |path: &Path| -> String {
        let rel = path
            .strip_prefix(&spec.root)
            .unwrap_or(path)
            .to_string_lossy();
        if cfg!(windows) {
            rel.chars()
                .map(|ch| if ch == '\\' { '/' } else { ch })
                .collect()
        } else {
            rel.into_owned()
        }
    };
    builder.build_parallel().run(|| {
        let mut sink = WorkerSink {
            set: TopSet::default(),
            shared: &shared,
        };
        let timed_out = &timed_out;
        let partial = &partial;
        let build_rel = &build_rel;
        let mut tick: u32 = 0;
        Box::new(move |entry| {
            // 2-1. 시간 예산 (시계 호출 32회당 1회)
            if timed_out.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            tick = tick.wrapping_add(1);
            if tick & 31 == 1 && deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                timed_out.store(true, Ordering::Relaxed);
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                partial.store(true, Ordering::Relaxed);
                return WalkState::Continue;
            };
            if entry.depth() == 0 {
                return WalkState::Continue;
            }

            // 2-2. 기본 제외·정렬 가지치기
            let file_type = entry.file_type();
            let is_dir = file_type.is_some_and(|kind| kind.is_dir());
            let is_file = file_type.is_some_and(|kind| kind.is_file());
            if is_dir && spec.default_excludes {
                let name = entry.file_name();
                if name == "node_modules"
                    || name == "target"
                    || (spec.include_hidden && name == ".git")
                {
                    return WalkState::Skip;
                }
            }
            let mut rel = None;
            if is_dir
                && spec.sort == SortKey::Path
                && let Some(floor) = &sink.set.floor
            {
                let probe = FoundEntry {
                    rank: 0,
                    rel: build_rel(entry.path()),
                    tail: String::new(),
                };
                if probe >= *floor {
                    return WalkState::Skip;
                }
                rel = Some(probe.rel);
            }
            if entry.depth() < spec.min_depth {
                return WalkState::Continue;
            }

            // 2-3. 이름 regex·glob
            if spec.full_path && rel.is_none() {
                rel = Some(build_rel(entry.path()));
            }
            let target_text = match &rel {
                Some(text) if spec.full_path => text.as_bytes(),
                _ => entry.file_name().as_encoded_bytes(),
            };
            if let Some(regex) = &spec.name_regex
                && !regex.is_match(target_text)
            {
                return WalkState::Continue;
            }
            if let Some(globs) = &spec.name_globs {
                let target = match &rel {
                    Some(text) if spec.full_path => Path::new(text.as_str()),
                    _ => Path::new(entry.file_name()),
                };
                if !globs.is_match(target) {
                    return WalkState::Continue;
                }
            }

            // 2-4. 확장자·종류·empty
            if !spec.extensions.is_empty() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if !spec
                    .extensions
                    .iter()
                    .any(|ext| name.ends_with(ext.as_str()))
                {
                    return WalkState::Continue;
                }
            }
            let kind_matches = match spec.kind {
                EntryKind::Any => true,
                EntryKind::File => is_file,
                EntryKind::Directory => is_dir,
                EntryKind::Symlink => entry.path_is_symlink(),
            };
            if !kind_matches {
                return WalkState::Continue;
            }
            let metadata = if needs_metadata {
                entry.metadata().ok()
            } else {
                None
            };
            if spec.empty_only {
                let empty = if is_dir {
                    std::fs::read_dir(entry.path())
                        .map(|mut children| children.next().is_none())
                        .unwrap_or(false)
                } else {
                    is_file && metadata.as_ref().is_some_and(|meta| meta.len() == 0)
                };
                if !empty {
                    return WalkState::Continue;
                }
            }

            // 2-5. 크기 (파일만)·수정 시각
            let size = if is_file {
                metadata.as_ref().map(std::fs::Metadata::len)
            } else {
                None
            };
            if !spec.sizes.is_empty() {
                let Some(size) = size else {
                    return WalkState::Continue;
                };
                let within = spec.sizes.iter().all(|bound| match *bound {
                    SizeBound::AtLeast(limit) => size >= limit,
                    SizeBound::AtMost(limit) => size <= limit,
                    SizeBound::Exactly(limit) => size == limit,
                });
                if !within {
                    return WalkState::Continue;
                }
            }
            let modified = metadata.as_ref().and_then(|meta| meta.modified().ok());
            if !spec.times.is_empty() {
                let Some(modified) = modified else {
                    return WalkState::Continue;
                };
                let within = spec.times.iter().all(|bound| match *bound {
                    TimeBound::After(limit) => modified > limit,
                    TimeBound::Before(limit) => modified < limit,
                });
                if !within {
                    return WalkState::Continue;
                }
            }

            // 2-6. 정렬 순위·표시 꼬리
            let rank = match spec.sort {
                SortKey::Path => 0,
                // 최신 우선 (시각 없음은 맨 뒤)
                SortKey::Modified => {
                    modified.map_or(i128::MAX, |time| match time.duration_since(UNIX_EPOCH) {
                        Ok(after) => -(after.as_nanos() as i128),
                        Err(before) => before.duration().as_nanos() as i128,
                    })
                }
                // 큰 파일 우선 (디렉터리·크기 없음은 맨 뒤)
                SortKey::Size => size.map_or(1, |bytes| -i128::from(bytes)),
            };
            let slash = if is_dir { "/" } else { "" };
            let tail = if spec.details {
                let stamp = modified.map_or_else(|| "-".to_string(), format_rfc3339_utc);
                match size {
                    Some(bytes) => format!("{slash}\t{bytes} B\t{stamp}"),
                    None => format!("{slash}\t{stamp}"),
                }
            } else {
                slash.to_string()
            };
            let found = FoundEntry {
                rank,
                rel: rel.unwrap_or_else(|| build_rel(entry.path())),
                tail,
            };
            keep_top_entry(&mut sink.set, found, spec.max_results, spec.byte_budget);
            WalkState::Continue
        })
    });

    // 3. 워커 집합 병합
    let sets = shared.into_inner().unwrap_or_else(PoisonError::into_inner);
    let merged = merge_top_sets(sets, spec.max_results, spec.byte_budget);
    Ok(FindOutcome {
        limit_reached: merged.clipped,
        entries: merged.heap.into_sorted_vec(),
        timed_out: timed_out.into_inner(),
        partial: partial.into_inner(),
    })
}
// 정렬 앞 페이지 유지 (최소 축출 키 기록)
fn keep_top_entry(set: &mut TopSet, found: FoundEntry, max_results: usize, byte_budget: usize) {
    // 축출 키 이후 항목 거부 (접두 보존)
    if set.floor.as_ref().is_some_and(|floor| found >= *floor) {
        set.clipped = true;
        return;
    }
    set.bytes += found.rel.len() + found.tail.len() + 1;
    set.heap.push(found);
    while set.heap.len() > max_results || set.bytes > byte_budget {
        let Some(evicted) = set.heap.pop() else {
            break;
        };
        set.bytes -= evicted.rel.len() + evicted.tail.len() + 1;
        set.clipped = true;
        if set.floor.as_ref().is_none_or(|floor| evicted < *floor) {
            set.floor = Some(evicted);
        }
    }
}
// 워커 집합 병합 (워커 최소 축출 키 상속)
fn merge_top_sets(sets: Vec<TopSet>, max_results: usize, byte_budget: usize) -> TopSet {
    let mut merged = TopSet::default();
    let mut heaps = Vec::with_capacity(sets.len());
    for set in sets {
        merged.clipped |= set.clipped;
        merged.floor = match (merged.floor.take(), set.floor) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        };
        heaps.push(set.heap);
    }
    for found in heaps.into_iter().flatten() {
        keep_top_entry(&mut merged, found, max_results, byte_budget);
    }
    merged
}
// SystemTime → RFC 3339 UTC 초 단위 (civil_from_days)
fn format_rfc3339_utc(time: SystemTime) -> String {
    let seconds = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => {
            let before = before.duration();
            let whole = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            -whole - i64::from(before.subsec_nanos() > 0)
        }
    };
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let second_of_day = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3_600,
        second_of_day / 60 % 60,
        second_of_day % 60
    )
}

// 5. Civil calendar and local time zone ---------------------------------------
// days_from_civil (Howard Hinnant)
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
// civil_from_days (Howard Hinnant) → (연, 월, 일)
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}
// 로컬 시간대 해석 실패 요지
const UNRESOLVED_LOCAL: &str = "the date could not be resolved in the local time zone";
// 로컬 벽시계 초 → (UTC 초, 적용 오프셋), fd 실측 later 규칙
#[cfg_attr(
    not(any(windows, all(unix, target_pointer_width = "64"))),
    allow(dead_code)
)]
fn pick_local_instant(
    local: i64,
    offset_at: &mut dyn FnMut(i64) -> Option<i64>,
) -> Option<(i64, i64)> {
    // 오프셋 탐침 폭 (최대 오프셋 초과)
    const ZONE_PROBE_SECONDS: i64 = 2 * 86_400;

    // 1. 전후 오프셋 후보
    let before = offset_at(local.checked_sub(ZONE_PROBE_SECONDS)?)?;
    let after = offset_at(local.checked_add(ZONE_PROBE_SECONDS)?)?;

    // 2. 겹침은 늦은 시각 (전이 후 오프셋)
    let mut picked: Option<(i64, i64)> = None;
    for offset in [before, after] {
        let instant = local.checked_sub(offset)?;
        if offset_at(instant)? == offset && picked.is_none_or(|(latest, _)| instant > latest) {
            picked = Some((instant, offset));
        }
    }

    // 3. 공백은 전이 전 오프셋 (시각 전진)
    picked.or_else(|| Some((local.checked_sub(before)?, before)))
}

#[cfg(windows)]
mod local_zone {
    //! Windows local zone like fd (jiff): TZ as TZif path, IANA name (system ICU), or POSIX rule,
    //! else the system zone through ICU IANA data with a kernel32 fallback.
    //! Without ICU (before 1703) a TZ zone name is an error, never a silent UTC.

    use super::{UNRESOLVED_LOCAL, civil_from_days, days_from_civil, pick_local_instant};
    use std::ffi::{CStr, OsStr, c_char, c_void};
    use std::mem::transmute;
    use std::path::Path;
    use std::ptr::{NonNull, null, null_mut};
    use std::sync::OnceLock;

    const SEARCH_SYSTEM32: u32 = 0x0000_0800;
    const ZONE_ID_INVALID: u32 = u32::MAX;
    const GREGORIAN_CALENDAR: i32 = 1;
    const ZONE_OFFSET_FIELD: i32 = 15;
    const DST_OFFSET_FIELD: i32 = 16;
    const ZONE_ID_CAPACITY: usize = 128;
    const FILETIME_EPOCH_SECONDS: i64 = 11_644_473_600;
    const FILETIME_TICKS: i64 = 10_000_000;
    // ICU 전용 Java 호환 ID (IANA 없음)
    const JAVA_ZONE_IDS: [&str; 25] = [
        "ACT", "AET", "AGT", "ART", "AST", "BET", "BST", "CAT", "CNT", "CST", "CTT", "EAT", "ECT",
        "IET", "IST", "JST", "MIT", "NET", "NST", "PLT", "PNT", "PRT", "PST", "SST", "VST",
    ];
    // tzdata 2024b backward 링크 (구 ICU 독립 zone)
    const LEGACY_ZONE_LINKS: [(&str, &str); 11] = [
        ("CET", "Europe/Brussels"),
        ("CST6CDT", "America/Chicago"),
        ("EET", "Europe/Athens"),
        ("EST", "America/Panama"),
        ("EST5EDT", "America/New_York"),
        ("HST", "Pacific/Honolulu"),
        ("MET", "Europe/Brussels"),
        ("MST", "America/Phoenix"),
        ("MST7MDT", "America/Denver"),
        ("PST8PDT", "America/Los_Angeles"),
        ("WET", "Europe/Lisbon"),
    ];

    // 1. Win32 and ICU declarations -------------------------------------------
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct SystemTimeFields {
        year: u16,
        month: u16,
        weekday: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        millisecond: u16,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTimeTicks {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct DynamicZoneInfo {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: SystemTimeFields,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: SystemTimeFields,
        daylight_bias: i32,
        key_name: [u16; 128],
        dynamic_disabled: u8,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDynamicTimeZoneInformation(info: *mut DynamicZoneInfo) -> u32;
        fn SystemTimeToTzSpecificLocalTimeEx(
            info: *const DynamicZoneInfo,
            utc: *const SystemTimeFields,
            local: *mut SystemTimeFields,
        ) -> i32;
        fn FileTimeToSystemTime(ticks: *const FileTimeTicks, fields: *mut SystemTimeFields) -> i32;
        fn SystemTimeToFileTime(fields: *const SystemTimeFields, ticks: *mut FileTimeTicks) -> i32;
        fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    type IcuOpen =
        unsafe extern "C" fn(*const u16, i32, *const c_char, i32, *mut i32) -> *mut c_void;
    type IcuClose = unsafe extern "C" fn(*mut c_void);
    type IcuSetMillis = unsafe extern "C" fn(*mut c_void, f64, *mut i32);
    type IcuGet = unsafe extern "C" fn(*const c_void, i32, *mut i32) -> i32;
    type IcuWindowsId =
        unsafe extern "C" fn(*const u16, i32, *const c_char, *mut u16, i32, *mut i32) -> i32;
    type IcuOpenIds = unsafe extern "C" fn(*mut i32) -> *mut c_void;
    type IcuNextId = unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> *const u16;
    pub(super) struct IcuCalendarApi {
        open: IcuOpen,
        close: IcuClose,
        set_millis: IcuSetMillis,
        get: IcuGet,
        windows_id: IcuWindowsId,
        open_ids: IcuOpenIds,
        next_id: IcuNextId,
        close_ids: IcuClose,
    }

    // 2. ICU calendar ---------------------------------------------------------
    // ICU 함수 표 (1903+ icu.dll, 1703-1809 icuin·icuuc 분리)
    pub(super) fn load_icu_api() -> Option<&'static IcuCalendarApi> {
        static ICU_API: OnceLock<Option<IcuCalendarApi>> = OnceLock::new();
        ICU_API
            .get_or_init(|| {
                open_icu_api(&["icu.dll"]).or_else(|| open_icu_api(&["icuin.dll", "icuuc.dll"]))
            })
            .as_ref()
    }
    // System32 ICU 모듈 묶음 → 함수 표 (심볼별 첫 보유 모듈)
    fn open_icu_api(libraries: &[&str]) -> Option<IcuCalendarApi> {
        let mut modules = Vec::with_capacity(libraries.len());
        for library in libraries {
            let name: Vec<u16> = library.encode_utf16().chain([0]).collect();
            // SAFETY: NUL 종료 UTF-16 이름, System32 한정 탐색 (DLL 바꿔치기 차단)
            let module = unsafe { LoadLibraryExW(name.as_ptr(), null_mut(), SEARCH_SYSTEM32) };
            if module.is_null() {
                return None;
            }
            modules.push(module);
        }
        let symbol = |symbol_name: &CStr| {
            modules.iter().find_map(|&module| {
                // SAFETY: 해제되지 않는 모듈 핸들, NUL 종료 심볼명
                let address = unsafe { GetProcAddress(module, symbol_name.as_ptr()) };
                (!address.is_null()).then_some(address)
            })
        };
        let open = symbol(c"ucal_open")?;
        let close = symbol(c"ucal_close")?;
        let set_millis = symbol(c"ucal_setMillis")?;
        let get = symbol(c"ucal_get")?;
        let windows_id = symbol(c"ucal_getTimeZoneIDForWindowsID")?;
        let open_ids = symbol(c"ucal_openTimeZones")?;
        let next_id = symbol(c"uenum_unext")?;
        let close_ids = symbol(c"uenum_close")?;
        // SAFETY: ICU4C C API 선언과 같은 시그니처 (ICU 모듈 내보내기, cdecl)
        unsafe {
            Some(IcuCalendarApi {
                open: transmute::<*mut c_void, IcuOpen>(open),
                close: transmute::<*mut c_void, IcuClose>(close),
                set_millis: transmute::<*mut c_void, IcuSetMillis>(set_millis),
                get: transmute::<*mut c_void, IcuGet>(get),
                windows_id: transmute::<*mut c_void, IcuWindowsId>(windows_id),
                open_ids: transmute::<*mut c_void, IcuOpenIds>(open_ids),
                next_id: transmute::<*mut c_void, IcuNextId>(next_id),
                close_ids: transmute::<*mut c_void, IcuClose>(close_ids),
            })
        }
    }
    // 열린 UCalendar 핸들 (Drop 시 ucal_close)
    struct IcuCalendar {
        api: &'static IcuCalendarApi,
        handle: NonNull<c_void>,
    }
    impl IcuCalendar {
        fn open_zone(api: &'static IcuCalendarApi, zone_id: &[u16]) -> Option<Self> {
            let length = i32::try_from(zone_id.len()).ok()?;
            let mut status = 0;
            // SAFETY: 길이 명시 UTF-16 ID, 기본 로캘(NULL), 지역 status 포인터
            let handle = unsafe {
                (api.open)(
                    zone_id.as_ptr(),
                    length,
                    null(),
                    GREGORIAN_CALENDAR,
                    &mut status,
                )
            };
            let calendar = Self {
                api,
                handle: NonNull::new(handle)?,
            };
            (status <= 0).then_some(calendar)
        }
        // TZ 이름 → IANA 시스템 ID (대소문자 무시)
        fn open_iana(api: &'static IcuCalendarApi, name: &str) -> Option<Self> {
            // 1. 2024b 링크 대상 치환
            let name = LEGACY_ZONE_LINKS
                .iter()
                .find(|(legacy, _)| legacy.eq_ignore_ascii_case(name))
                .map_or(name, |&(_, target)| target);

            // 2. ICU 시스템 ID 열거 (정확 표기 탐색)
            let mut status = 0;
            // SAFETY: 지역 status 포인터
            let ids = NonNull::new(unsafe { (api.open_ids)(&mut status) })?;
            let mut zone_id = None;
            while status <= 0 {
                let mut length = 0;
                // SAFETY: 열린 열거 핸들, 지역 길이·status 포인터
                let units = unsafe { (api.next_id)(ids.as_ptr(), &mut length, &mut status) };
                let Ok(length) = usize::try_from(length) else {
                    break;
                };
                if units.is_null() || status > 0 {
                    break;
                }
                // SAFETY: 다음 uenum_unext 전까지 유효한 length 길이 UTF-16 ID
                let units = unsafe { std::slice::from_raw_parts(units, length) };
                if let Ok(id) = String::from_utf16(units)
                    && id.eq_ignore_ascii_case(name)
                {
                    zone_id = Some(id);
                    break;
                }
            }
            // SAFETY: ucal_openTimeZones 성공 핸들, 1회 해제
            unsafe { (api.close_ids)(ids.as_ptr()) };

            // 3. Java 호환·SystemV ID 제외
            let zone_id = zone_id?;
            if JAVA_ZONE_IDS.contains(&zone_id.as_str()) || zone_id.starts_with("SystemV/") {
                return None;
            }
            let zone_id: Vec<u16> = zone_id.encode_utf16().collect();
            Self::open_zone(api, &zone_id)
        }
        // Windows 시간대 키 → IANA ID (CLDR 001 지역)
        fn open_windows(api: &'static IcuCalendarApi, key_name: &[u16]) -> Option<Self> {
            let key_length = key_name.iter().position(|&unit| unit == 0)?;
            let mut zone_id = [0_u16; ZONE_ID_CAPACITY];
            let mut status = 0;
            // SAFETY: 길이 명시 입력, 지역 기본값(NULL), 용량 명시 출력 버퍼
            let length = unsafe {
                (api.windows_id)(
                    key_name.as_ptr(),
                    i32::try_from(key_length).ok()?,
                    null(),
                    zone_id.as_mut_ptr(),
                    ZONE_ID_CAPACITY as i32,
                    &mut status,
                )
            };
            if status > 0 {
                return None;
            }
            let zone_id = zone_id.get(..usize::try_from(length).ok()?)?;
            if zone_id.is_empty() {
                return None;
            }
            Self::open_zone(api, zone_id)
        }
        // UTC 초 → 총 오프셋 초 (표준 + DST)
        fn offset_at(&self, seconds: i64) -> Option<i64> {
            let mut status = 0;
            let handle = self.handle.as_ptr();
            // SAFETY: 열린 달력 핸들, 지역 status 포인터
            let (zone, daylight) = unsafe {
                (self.api.set_millis)(handle, seconds as f64 * 1_000.0, &mut status);
                (
                    (self.api.get)(handle, ZONE_OFFSET_FIELD, &mut status),
                    (self.api.get)(handle, DST_OFFSET_FIELD, &mut status),
                )
            };
            (status <= 0).then(|| (i64::from(zone) + i64::from(daylight)).div_euclid(1_000))
        }
    }
    impl Drop for IcuCalendar {
        fn drop(&mut self) {
            // SAFETY: open_zone 성공 핸들, 1회 해제
            unsafe { (self.api.close)(self.handle.as_ptr()) };
        }
    }

    // 3. POSIX TZ rule --------------------------------------------------------
    struct PosixZone {
        standard: i64,
        daylight: Option<PosixDaylight>,
    }
    struct PosixDaylight {
        offset: i64,
        start: PosixRule,
        end: PosixRule,
    }
    struct PosixRule {
        day: PosixDay,
        time: i64,
    }
    enum PosixDay {
        Julian(i64),
        ZeroBased(i64),
        Weekday { month: i64, week: i64, weekday: i64 },
    }
    // std offset [dst [offset] ,start[/time],end[/time]] (DST 규칙 필수, jiff 동일)
    fn parse_posix_zone(text: &str) -> Option<PosixZone> {
        fn skip_name(bytes: &[u8], at: &mut usize) -> Option<()> {
            let rest = bytes.get(*at..)?;
            if rest.first() == Some(&b'<') {
                let close = rest.iter().position(|&byte| byte == b'>')?;
                let inner = rest.get(1..close)?;
                let allowed =
                    |byte: &u8| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-');
                if inner.len() < 3 || !inner.iter().all(allowed) {
                    return None;
                }
                *at += close + 1;
            } else {
                let length = rest
                    .iter()
                    .take_while(|byte| byte.is_ascii_alphabetic())
                    .count();
                if length < 3 {
                    return None;
                }
                *at += length;
            }
            Some(())
        }
        fn read_number(bytes: &[u8], at: &mut usize, max_digits: usize) -> Option<i64> {
            let rest = bytes.get(*at..)?;
            let digits = rest
                .iter()
                .take(max_digits)
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            let value = std::str::from_utf8(rest.get(..digits)?)
                .ok()?
                .parse()
                .ok()?;
            *at += digits;
            Some(value)
        }
        // [+-]h[hh][:mm[:ss]] → 부호 있는 초
        fn read_clock(bytes: &[u8], at: &mut usize, max_hours: i64) -> Option<i64> {
            let negative = bytes.get(*at) == Some(&b'-');
            if matches!(bytes.get(*at), Some(b'-' | b'+')) {
                *at += 1;
            }
            let hours = read_number(bytes, at, 3)?;
            if hours > max_hours {
                return None;
            }
            let mut total = hours * 3_600;
            for scale in [60, 1] {
                if bytes.get(*at) != Some(&b':') {
                    break;
                }
                *at += 1;
                let start = *at;
                let part = read_number(bytes, at, 2)?;
                if *at - start != 2 || part > 59 {
                    return None;
                }
                total += part * scale;
            }
            Some(if negative { -total } else { total })
        }
        fn read_rule(bytes: &[u8], at: &mut usize) -> Option<PosixRule> {
            let day = match bytes.get(*at) {
                Some(b'J') => {
                    *at += 1;
                    let day = read_number(bytes, at, 3)?;
                    (1..=365).contains(&day).then_some(PosixDay::Julian(day))?
                }
                Some(b'M') => {
                    *at += 1;
                    let month = read_number(bytes, at, 2)?;
                    let mut parts = [month, 0, 0];
                    for part in parts.iter_mut().skip(1) {
                        if bytes.get(*at) != Some(&b'.') {
                            return None;
                        }
                        *at += 1;
                        *part = read_number(bytes, at, 1)?;
                    }
                    let [month, week, weekday] = parts;
                    let valid = (1..=12).contains(&month)
                        && (1..=5).contains(&week)
                        && (0..=6).contains(&weekday);
                    valid.then_some(PosixDay::Weekday {
                        month,
                        week,
                        weekday,
                    })?
                }
                _ => {
                    let day = read_number(bytes, at, 3)?;
                    (day <= 365).then_some(PosixDay::ZeroBased(day))?
                }
            };
            let time = if bytes.get(*at) == Some(&b'/') {
                *at += 1;
                read_clock(bytes, at, 167)?
            } else {
                7_200
            };
            Some(PosixRule { day, time })
        }

        // 1. 표준시 이름·오프셋 (POSIX 부호 = 서쪽 양수)
        let bytes = text.as_bytes();
        let mut at = 0;
        skip_name(bytes, &mut at)?;
        let standard = -read_clock(bytes, &mut at, 24)?;
        if at == bytes.len() {
            return Some(PosixZone {
                standard,
                daylight: None,
            });
        }

        // 2. DST 이름·오프셋 (기본 표준 + 1h)
        skip_name(bytes, &mut at)?;
        let offset = if bytes.get(at) == Some(&b',') {
            standard + 3_600
        } else {
            -read_clock(bytes, &mut at, 24)?
        };

        // 3. 시작·종료 규칙
        if bytes.get(at) != Some(&b',') {
            return None;
        }
        at += 1;
        let start = read_rule(bytes, &mut at)?;
        if bytes.get(at) != Some(&b',') {
            return None;
        }
        at += 1;
        let end = read_rule(bytes, &mut at)?;
        (at == bytes.len()).then_some(PosixZone {
            standard,
            daylight: Some(PosixDaylight { offset, start, end }),
        })
    }
    // UTC 초 → POSIX 규칙 오프셋 (시작 표준시, 종료 DST 벽시계)
    fn posix_offset_at(zone: &PosixZone, seconds: i64) -> i64 {
        let Some(daylight) = &zone.daylight else {
            return zone.standard;
        };
        // 1. 표준시 기준 연도
        let (year, _, _) = civil_from_days((seconds + zone.standard).div_euclid(86_400));
        let rule_days = |rule: &PosixRule| match rule.day {
            PosixDay::Julian(day) => {
                let leap = days_from_civil(year, 3, 1) - days_from_civil(year, 2, 28) == 2;
                days_from_civil(year, 1, 1) + day - 1 + i64::from(leap && day >= 60)
            }
            PosixDay::ZeroBased(day) => days_from_civil(year, 1, 1) + day,
            PosixDay::Weekday {
                month,
                week,
                weekday,
            } => {
                // 1970-01-01 목요일 기준 요일, 5주차는 마지막 주
                let first = days_from_civil(year, month, 1);
                let next = days_from_civil(year + month / 12, month % 12 + 1, 1);
                let mut day = first + (weekday - (first + 4).rem_euclid(7)).rem_euclid(7);
                day += 7 * (week - 1);
                while day >= next {
                    day -= 7;
                }
                day
            }
        };

        // 2. 전이 시점 비교 (남반구는 연말 걸침)
        let start = rule_days(&daylight.start) * 86_400 + daylight.start.time - zone.standard;
        let end = rule_days(&daylight.end) * 86_400 + daylight.end.time - daylight.offset;
        let in_daylight = if start < end {
            start <= seconds && seconds < end
        } else {
            !(end <= seconds && seconds < start)
        };
        if in_daylight {
            daylight.offset
        } else {
            zone.standard
        }
    }

    // 4. TZif file ------------------------------------------------------------
    struct TzifZone {
        transitions: Vec<i64>,
        type_indexes: Vec<usize>,
        offsets: Vec<i64>,
        footer: Option<PosixZone>,
    }
    // RFC 8536 TZif 해석 (윤초 무시)
    fn parse_tzif(bytes: &[u8]) -> Option<TzifZone> {
        // 1. 헤더 개수 (isut, isstd, leap, time, type, char)
        let read_counts = |at: usize| -> Option<[usize; 6]> {
            let header = bytes.get(at..at.checked_add(44)?)?;
            if header.get(..4)? != b"TZif" {
                return None;
            }
            let mut counts = [0; 6];
            for (count, raw) in counts.iter_mut().zip(header.get(20..44)?.chunks_exact(4)) {
                *count = usize::try_from(u32::from_be_bytes(raw.try_into().ok()?)).ok()?;
            }
            Some(counts)
        };
        let block_len = |counts: [usize; 6], time_size: usize| -> Option<usize> {
            let [is_ut, is_std, leap, time, types, chars] = counts;
            let mut total = time.checked_mul(time_size + 1)?;
            for part in [
                types.checked_mul(6)?,
                chars,
                leap.checked_mul(time_size + 4)?,
                is_std,
                is_ut,
            ] {
                total = total.checked_add(part)?;
            }
            Some(total)
        };

        // 2. v2+ 는 두번째 헤더·블록 사용
        let mut counts = read_counts(0)?;
        let (mut start, mut time_size): (usize, usize) = (44, 4);
        if *bytes.get(4)? != 0 {
            start = start.checked_add(block_len(counts, 4)?)?;
            counts = read_counts(start)?;
            start += 44;
            time_size = 8;
        }
        let [_, _, _, time_count, type_count, _] = counts;
        let block_end = start.checked_add(block_len(counts, time_size)?)?;
        let block = bytes.get(start..block_end)?;
        if type_count == 0 {
            return None;
        }

        // 3. 전이 시각·종류 색인·종류 오프셋
        let (times, rest) = block.split_at_checked(time_count * time_size)?;
        let (indexes, rest) = rest.split_at_checked(time_count)?;
        // 부호 확장 big-endian 정수
        let transitions: Vec<i64> = times
            .chunks_exact(time_size)
            .map(|raw| {
                let sign = i64::from(raw.first().is_some_and(|&byte| byte >= 0x80)).wrapping_neg();
                raw.iter()
                    .fold(sign, |total, &byte| (total << 8) | i64::from(byte))
            })
            .collect();
        if transitions
            .windows(2)
            .any(|pair| pair.first() >= pair.get(1))
        {
            return None;
        }
        let type_indexes: Vec<usize> = indexes.iter().map(|&index| usize::from(index)).collect();
        if type_indexes.iter().any(|&index| index >= type_count) {
            return None;
        }
        let mut offsets = Vec::with_capacity(type_count);
        for record in rest.get(..type_count * 6)?.chunks_exact(6) {
            let offset = i64::from(i32::from_be_bytes(record.get(..4)?.try_into().ok()?));
            if offset.abs() >= 26 * 3_600 {
                return None;
            }
            offsets.push(offset);
        }

        // 4. 바닥글 POSIX 규칙 (\n...\n, 빈 값 허용)
        let mut footer = None;
        if time_size == 8 {
            let text = bytes.get(block_end..)?.strip_prefix(b"\n")?;
            let rule = text.get(..text.iter().position(|&byte| byte == b'\n')?)?;
            if !rule.is_empty() {
                footer = Some(parse_posix_zone(std::str::from_utf8(rule).ok()?)?);
            }
        }
        Some(TzifZone {
            transitions,
            type_indexes,
            offsets,
            footer,
        })
    }
    // UTC 초 → TZif 오프셋
    fn tzif_offset_at(zone: &TzifZone, seconds: i64) -> Option<i64> {
        let passed = zone.transitions.partition_point(|&at| at <= seconds);
        if passed == zone.transitions.len()
            && let Some(footer) = &zone.footer
        {
            return Some(posix_offset_at(footer, seconds));
        }
        let type_index = match passed.checked_sub(1) {
            Some(last) => *zone.type_indexes.get(last)?,
            None => 0,
        };
        zone.offsets.get(type_index).copied()
    }

    // 5. Zone selection -------------------------------------------------------
    // kernel32 동적 DST 규칙 오프셋 (ICU 부재 대체)
    fn dynamic_offset_at(info: &DynamicZoneInfo, seconds: i64) -> Option<i64> {
        let ticks = seconds
            .checked_add(FILETIME_EPOCH_SECONDS)?
            .checked_mul(FILETIME_TICKS)?;
        let raw = u64::try_from(ticks).ok()?;
        let utc_ticks = FileTimeTicks {
            low: (raw & 0xFFFF_FFFF) as u32,
            high: (raw >> 32) as u32,
        };
        let mut utc = SystemTimeFields::default();
        let mut local = SystemTimeFields::default();
        let mut local_ticks = FileTimeTicks::default();
        // SAFETY: 모두 유효 지역 변수 포인터
        let converted = unsafe {
            FileTimeToSystemTime(&utc_ticks, &mut utc) != 0
                && SystemTimeToTzSpecificLocalTimeEx(info, &utc, &mut local) != 0
                && SystemTimeToFileTime(&local, &mut local_ticks) != 0
        };
        if !converted {
            return None;
        }
        let local_raw = (u64::from(local_ticks.high) << 32) | u64::from(local_ticks.low);
        let local_ticks = i64::try_from(local_raw).ok()?;
        Some((local_ticks - ticks).div_euclid(FILETIME_TICKS))
    }
    // 시스템 시간대 정보 (키 이름·동적 DST)
    fn read_system_zone() -> Option<DynamicZoneInfo> {
        let mut info = DynamicZoneInfo {
            bias: 0,
            standard_name: [0; 32],
            standard_date: SystemTimeFields::default(),
            standard_bias: 0,
            daylight_name: [0; 32],
            daylight_date: SystemTimeFields::default(),
            daylight_bias: 0,
            key_name: [0; 128],
            dynamic_disabled: 0,
        };
        // SAFETY: 쓰기 가능한 지역 구조체 포인터
        let zone_id = unsafe { GetDynamicTimeZoneInformation(&mut info) };
        (zone_id != ZONE_ID_INVALID).then_some(info)
    }
    // TZ 값 기준 해석 (None = 미설정, 시스템 시간대)
    pub(super) fn resolve_with_tz(
        local: i64,
        tz: Option<&OsStr>,
        icu: Option<&'static IcuCalendarApi>,
    ) -> Result<(i64, i64), String> {
        let unresolved = || UNRESOLVED_LOCAL.to_string();

        // 1. TZ: TZif 절대 경로 → IANA → POSIX 규칙 → 해석 불가 UTC (jiff 동일)
        if let Some(value) = tz {
            let text = value.to_str().unwrap_or_default();
            let (name, colon) = match text.strip_prefix(':') {
                Some(rest) => (rest, true),
                None => (text, false),
            };
            if Path::new(name).is_absolute() {
                let Some(zone) = std::fs::read(name)
                    .ok()
                    .and_then(|bytes| parse_tzif(&bytes))
                else {
                    return Ok((local, 0));
                };
                return pick_local_instant(local, &mut |seconds| tzif_offset_at(&zone, seconds))
                    .ok_or_else(unresolved);
            }
            if let Some(api) = icu
                && let Some(calendar) = IcuCalendar::open_iana(api, name)
            {
                return pick_local_instant(local, &mut |seconds| calendar.offset_at(seconds))
                    .ok_or_else(unresolved);
            }
            if !colon && let Some(zone) = parse_posix_zone(name) {
                return pick_local_instant(local, &mut |seconds| {
                    Some(posix_offset_at(&zone, seconds))
                })
                .ok_or_else(unresolved);
            }
            // 오프셋 0 고정 이름 (ICU 불필요)
            let zero_offset = [
                "UTC",
                "UCT",
                "GMT",
                "GMT0",
                "GMT+0",
                "GMT-0",
                "Zulu",
                "Universal",
                "Greenwich",
            ]
            .iter()
            .any(|zone| {
                name.eq_ignore_ascii_case(zone)
                    || name
                        .get(..4)
                        .is_some_and(|head| head.eq_ignore_ascii_case("Etc/"))
                        && name
                            .get(4..)
                            .is_some_and(|tail| tail.eq_ignore_ascii_case(zone))
            });
            if zero_offset {
                return Ok((local, 0));
            }
            // ICU 부재 이름 (UTC 대체 금지)
            if icu.is_none() && !name.is_empty() {
                return Err(format!(
                    "the TZ value '{text}' names a time zone that needs the Windows ICU library (icu.dll on Windows 10 1903+, icuin.dll and icuuc.dll on 1703-1809), which this host lacks; set TZ to a POSIX rule such as KST-9 or unset it"
                ));
            }
            return Ok((local, 0));
        }

        // 2. 시스템 시간대 (ICU IANA 이력, 없으면 kernel32 규칙)
        let info = read_system_zone().ok_or_else(unresolved)?;
        if let Some(api) = icu
            && let Some(calendar) = IcuCalendar::open_windows(api, &info.key_name)
        {
            return pick_local_instant(local, &mut |seconds| calendar.offset_at(seconds))
                .ok_or_else(unresolved);
        }
        pick_local_instant(local, &mut |seconds| dynamic_offset_at(&info, seconds))
            .ok_or_else(unresolved)
    }
    // 오프셋 없는 로컬 시각 → (UTC 초, 적용 오프셋)
    pub(super) fn local_to_utc_seconds(local: i64) -> Result<(i64, i64), String> {
        resolve_with_tz(local, std::env::var_os("TZ").as_deref(), load_icu_api())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn icu_and_kernel32_agree_on_current_rules() {
            let Some(api) = load_icu_api() else {
                return;
            };
            let info = read_system_zone().unwrap();
            let calendar = IcuCalendar::open_windows(api, &info.key_name).unwrap();
            // 2026 년 10일 간격 표본
            let year_start = days_from_civil(2026, 1, 1) * 86_400;
            for step in 0..37 {
                let seconds = year_start + step * 10 * 86_400 + 3_600;
                assert_eq!(
                    calendar.offset_at(seconds),
                    dynamic_offset_at(&info, seconds),
                    "{seconds}"
                );
            }
        }
        #[test]
        fn iana_names_exclude_custom_and_java_ids() {
            let Some(api) = load_icu_api() else {
                return;
            };
            let offset = |name: &str| {
                IcuCalendar::open_iana(api, name).and_then(|calendar| calendar.offset_at(0))
            };
            assert_eq!(offset("Asia/Tokyo"), Some(32_400));
            assert_eq!(offset("America/New_York"), Some(-18_000));
            assert_eq!(offset("UTC"), Some(0));
            for name in ["GMT+9", "JST", "PST", "garbage", "", "SystemV/EST5"] {
                assert_eq!(offset(name), None, "{name}");
            }
        }
        #[test]
        fn iana_tz_without_icu_is_an_error_naming_the_value() {
            let local = days_from_civil(2026, 7, 1) * 86_400;
            let resolve = |value: &str, icu| resolve_with_tz(local, Some(OsStr::new(value)), icu);
            // ICU 부재 주입: UTC 대체 금지
            for value in ["Asia/Seoul", ":Asia/Seoul", "ROK"] {
                let error = resolve(value, None).unwrap_err();
                assert!(
                    error.contains(&format!("TZ value '{value}'")) && error.contains("icu.dll"),
                    "{error}"
                );
            }
            // ICU 불필요 값 유지
            assert_eq!(resolve("KST-9", None), Ok((local - 32_400, 32_400)));
            assert_eq!(resolve("", None), Ok((local, 0)));
            // 오프셋 0 이름은 ICU 없이 UTC
            for value in ["UTC", ":Etc/UTC", "GMT", "etc/gmt0", "Zulu", "Universal"] {
                assert_eq!(resolve(value, None), Ok((local, 0)), "{value}");
            }
            if let Some(api) = load_icu_api() {
                assert_eq!(
                    resolve("Asia/Seoul", Some(api)),
                    Ok((local - 32_400, 32_400))
                );
            }
        }
        #[test]
        fn split_icu_modules_load_the_calendar_api() {
            assert!(open_icu_api(&["rust-fs-mcp-missing-icu.dll"]).is_none());
            let system32 = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32");
            if !system32.join("icuin.dll").is_file() || !system32.join("icuuc.dll").is_file() {
                return;
            }
            // 1703-1809 분리 모듈 경로
            let api = Box::leak(Box::new(open_icu_api(&["icuin.dll", "icuuc.dll"]).unwrap()));
            let calendar = IcuCalendar::open_iana(api, "Asia/Tokyo").unwrap();
            assert_eq!(calendar.offset_at(0), Some(32_400));
        }
        #[test]
        fn posix_rules_parse_and_evaluate() {
            let at = |text: &str, date: (i64, i64, i64), hour: i64| {
                let zone = parse_posix_zone(text).unwrap();
                let seconds = days_from_civil(date.0, date.1, date.2) * 86_400 + hour * 3_600;
                posix_offset_at(&zone, seconds)
            };
            // 북반구 US 규칙 (3월 둘째 일요일 07:00Z 전후)
            let us = "EST5EDT,M3.2.0,M11.1.0";
            assert_eq!(at(us, (2026, 1, 15), 12), -18_000);
            assert_eq!(at(us, (2026, 7, 1), 12), -14_400);
            assert_eq!(at(us, (2026, 3, 8), 6), -18_000);
            assert_eq!(at(us, (2026, 3, 8), 7), -14_400);
            assert_eq!(at(us, (2026, 11, 1), 5), -14_400);
            assert_eq!(at(us, (2026, 11, 1), 6), -18_000);
            // 남반구·분 오프셋·Julian·고정 오프셋
            let au = "AEST-10AEDT,M10.1.0,M4.1.0/3";
            assert_eq!(at(au, (2026, 1, 1), 0), 39_600);
            assert_eq!(at(au, (2026, 7, 1), 0), 36_000);
            assert_eq!(at("<+0330>-3:30", (2026, 7, 1), 0), 12_600);
            assert_eq!(at("ABC5DEF,J60/0,J61/0", (2024, 3, 1), 12), -14_400);
            assert_eq!(at("ABC5DEF,J60/0,J61/0", (2024, 2, 29), 12), -18_000);
            assert_eq!(at("ABC+5:30", (2026, 7, 1), 0), -19_800);
            assert_eq!(at("ABC24", (2026, 7, 1), 0), -86_400);
            for bad in [
                "ABC5DEF",
                "ABC5DEF4",
                "AB5",
                "ABC25",
                "ABC",
                "ABC5:3",
                "<AB>5",
                "ABC5DEF,M13.1.0,M11.1.0",
                "ABC5DEF,M3.6.0,M11.1.0",
                "ABC5DEF,M3.2.0",
                "ABC5DEF,J0,J100",
                "ABC5DEF,366,1",
                "ABC5DEF,M3.2.0/168,M11.1.0",
                "ABC5 ",
            ] {
                assert!(parse_posix_zone(bad).is_none(), "{bad}");
            }
        }
        #[test]
        fn tzif_offsets_and_truncation() {
            let bytes = super::super::tests::sample_tzif();
            let zone = parse_tzif(&bytes).unwrap();
            let at = |date: (i64, i64, i64)| {
                tzif_offset_at(&zone, days_from_civil(date.0, date.1, date.2) * 86_400)
            };
            // 첫 전이 전·표 구간·바닥글 규칙
            assert_eq!(at((1950, 7, 1)), Some(1_800));
            assert_eq!(at((1965, 7, 1)), Some(3_600));
            assert_eq!(at((1970, 7, 1)), Some(7_200));
            assert_eq!(at((2026, 1, 15)), Some(3_600));
            assert_eq!(at((2026, 7, 1)), Some(7_200));
            // 잘린 입력·v1 단독
            for cut in 0..bytes.len() - 1 {
                assert!(parse_tzif(bytes.get(..cut).unwrap()).is_none(), "{cut}");
            }
            let mut version_one = bytes.get(..44 + 3 * 5 + 3 * 6 + 12).unwrap().to_vec();
            version_one[4] = 0;
            let zone = parse_tzif(&version_one).unwrap();
            assert!(zone.footer.is_none());
            assert_eq!(tzif_offset_at(&zone, 40_000_000), Some(3_600));
        }
    }
}

#[cfg(all(unix, target_pointer_width = "64"))]
mod local_zone {
    //! Unix: libc localtime_r (TZ, else /etc/localtime, same tzdb as jiff) probed per instant.

    use super::{UNRESOLVED_LOCAL, days_from_civil, pick_local_instant};
    use std::ffi::c_int;

    // struct tm 앞 9필드 + 플랫폼 꼬리 여유
    #[repr(C)]
    struct CivilTm {
        second: c_int,
        minute: c_int,
        hour: c_int,
        day: c_int,
        month: c_int,
        year: c_int,
        weekday: c_int,
        year_day: c_int,
        is_dst: c_int,
        tail: [i64; 8],
    }
    unsafe extern "C" {
        fn tzset();
        fn localtime_r(time: *const i64, out: *mut CivilTm) -> *mut CivilTm;
    }
    // 오프셋 없는 로컬 시각 → (UTC 초, 적용 오프셋)
    pub(super) fn local_to_utc_seconds(local: i64) -> Result<(i64, i64), String> {
        // SAFETY: 인자 없는 libc TZ 재적재
        unsafe { tzset() };
        pick_local_instant(local, &mut |seconds| {
            let mut civil = CivilTm {
                second: 0,
                minute: 0,
                hour: 0,
                day: 0,
                month: 0,
                year: 0,
                weekday: 0,
                year_day: 0,
                is_dst: 0,
                tail: [0; 8],
            };
            // SAFETY: 64비트 time_t 입력, struct tm 보다 큰 지역 출력 버퍼
            let filled = unsafe { localtime_r(&seconds, &mut civil) };
            if filled.is_null() {
                return None;
            }
            let days = days_from_civil(
                i64::from(civil.year) + 1_900,
                i64::from(civil.month) + 1,
                i64::from(civil.day),
            );
            let wall = days * 86_400
                + i64::from(civil.hour) * 3_600
                + i64::from(civil.minute) * 60
                + i64::from(civil.second);
            Some(wall - seconds)
        })
        .ok_or_else(|| UNRESOLVED_LOCAL.to_string())
    }
}

#[cfg(not(any(windows, all(unix, target_pointer_width = "64"))))]
mod local_zone {
    //! Other targets: no local time zone source; offset-free dates are rejected.

    use super::UNRESOLVED_LOCAL;

    // 로컬 시간대 미지원 플랫폼
    pub(super) fn local_to_utc_seconds(_local: i64) -> Result<(i64, i64), String> {
        Err(UNRESOLVED_LOCAL.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(prefix: &str) -> PathBuf {
        // target/ 밖이어야 기본 제외 판정 유지
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn write_file(root: &Path, rel: &str, body: &[u8]) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    fn run_find(root: &Path, extra: Value) -> RawResult {
        let mut item = json!({ "path": root.display().to_string() });
        for (key, value) in extra.as_object().unwrap() {
            item[key] = value.clone();
        }
        find_item(&item)
    }
    fn result_text(result: &RawResult) -> String {
        result.content[0]["text"].as_str().unwrap().to_string()
    }
    fn listed(root: &Path, extra: Value) -> Vec<String> {
        let result = run_find(root, extra);
        assert!(!result.is_error, "{}", result_text(&result));
        result_text(&result)
            .lines()
            .skip(1)
            .filter(|line| !line.starts_with('[') && *line != "(no matches)")
            .map(str::to_string)
            .collect()
    }
    fn sorted(mut lines: Vec<String>) -> Vec<String> {
        lines.sort();
        lines
    }
    fn set_mtime(path: &Path, time: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }
    fn sample_tree(prefix: &str) -> PathBuf {
        let root = temp_dir(prefix);
        write_file(&root, "Main.rs", b"fn main() {}\n");
        write_file(&root, "main.txt", b"hello\n");
        write_file(&root, "notes.TOML", b"a = 1\n");
        write_file(&root, "src/lib.rs", b"pub fn lib() {}\n");
        write_file(&root, "src/deep/a.rs", b"// a\n");
        write_file(&root, "src/deep/b.md", b"# b\n");
        root
    }
    #[test]
    fn regex_uses_smart_case_unless_forced() {
        let root = sample_tree("rfm-find-smart");
        assert_eq!(
            listed(&root, json!({ "pattern": "^main" })),
            vec!["Main.rs", "main.txt"]
        );
        assert_eq!(
            listed(&root, json!({ "pattern": "^Main" })),
            vec!["Main.rs"]
        );
        assert_eq!(
            listed(&root, json!({ "pattern": "^main", "ignoreCase": false })),
            vec!["main.txt"]
        );
        let broken = run_find(&root, json!({ "pattern": "main(" }));
        assert!(broken.is_error);
        assert!(result_text(&broken).contains("Invalid pattern regex"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn glob_matches_names_with_alternatives() {
        let root = sample_tree("rfm-find-glob");
        assert_eq!(
            listed(&root, json!({ "glob": "*.rs" })),
            vec!["Main.rs", "src/deep/a.rs", "src/lib.rs"]
        );
        assert_eq!(
            listed(&root, json!({ "glob": "*.md|*.toml" })),
            vec!["notes.TOML", "src/deep/b.md"]
        );
        // 대문자 glob 은 대소문자 구분
        assert!(listed(&root, json!({ "glob": "*.Toml" })).is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn full_path_matches_root_relative_path() {
        let root = sample_tree("rfm-find-full");
        assert_eq!(
            listed(&root, json!({ "glob": "src/**/*.rs", "fullPath": true })),
            vec!["src/deep/a.rs", "src/lib.rs"]
        );
        // literal_separator ('/' 비통과)
        assert_eq!(
            listed(&root, json!({ "glob": "*.rs", "fullPath": true })),
            vec!["Main.rs"]
        );
        assert_eq!(
            listed(&root, json!({ "pattern": "^src/deep/", "fullPath": true })),
            vec!["src/deep/a.rs", "src/deep/b.md"]
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn type_filter_and_directory_slash() {
        let root = sample_tree("rfm-find-type");
        assert_eq!(
            listed(&root, json!({ "type": "directory" })),
            vec!["src/", "src/deep/"]
        );
        assert_eq!(listed(&root, json!({ "type": "file" })).len(), 6);
        let bad = run_find(&root, json!({ "type": "socket" }));
        assert!(result_text(&bad).contains("use any, file, directory, or symlink"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn empty_lists_empty_files_and_directories() {
        let root = sample_tree("rfm-find-empty");
        write_file(&root, "blank.txt", b"");
        fs::create_dir_all(root.join("hollow")).unwrap();
        assert_eq!(
            listed(&root, json!({ "empty": true })),
            vec!["blank.txt", "hollow/"]
        );
        assert_eq!(
            listed(&root, json!({ "empty": true, "type": "file" })),
            vec!["blank.txt"]
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn extension_is_case_insensitive_list() {
        let root = sample_tree("rfm-find-ext");
        assert_eq!(
            listed(&root, json!({ "extension": "RS|toml" })),
            vec!["Main.rs", "notes.TOML", "src/deep/a.rs", "src/lib.rs"]
        );
        assert_eq!(
            listed(&root, json!({ "extension": ".md,txt" })),
            vec!["main.txt", "src/deep/b.md"]
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn size_bounds_follow_fd_units() {
        let root = temp_dir("rfm-find-size");
        write_file(&root, "tiny.bin", &[0; 10]);
        write_file(&root, "mid.bin", &[0; 2_000]);
        write_file(&root, "big.bin", &[0; 5_000]);
        write_file(&root, "dir/kib.bin", &[0; 1_024]);
        assert_eq!(
            listed(&root, json!({ "size": "+1k" })),
            vec!["big.bin", "dir/kib.bin", "mid.bin"]
        );
        assert_eq!(
            listed(&root, json!({ "size": "-2k" })),
            vec!["dir/kib.bin", "mid.bin", "tiny.bin"]
        );
        assert_eq!(
            listed(&root, json!({ "size": "+1k, -3kb" })),
            vec!["dir/kib.bin", "mid.bin"]
        );
        assert_eq!(listed(&root, json!({ "size": "1ki" })), vec!["dir/kib.bin"]);
        assert_eq!(listed(&root, json!({ "size": "10B" })), vec!["tiny.bin"]);
        for bad in ["10", "+1x", "k", "+-1k", "1k,", "99999999999999999999t"] {
            let result = run_find(&root, json!({ "size": bad }));
            assert!(result.is_error, "{bad}");
            assert!(result_text(&result).contains("Invalid size"), "{bad}");
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn changed_within_and_before_use_mtime() {
        let root = temp_dir("rfm-find-time");
        write_file(&root, "old.txt", b"old\n");
        write_file(&root, "new.txt", b"new\n");
        let ten_days = Duration::from_secs(10 * 86_400);
        set_mtime(&root.join("old.txt"), SystemTime::now() - ten_days);
        let item = json!({ "type": "file", "changedWithin": "1day" });
        assert_eq!(listed(&root, item), vec!["new.txt"]);
        let item = json!({ "type": "file", "changedBefore": "2d 12h" });
        assert_eq!(listed(&root, item), vec!["old.txt"]);
        let item = json!({ "type": "file", "changedWithin": "2weeks", "changedBefore": "1w" });
        assert_eq!(listed(&root, item), vec!["old.txt"]);
        let item = json!({ "type": "file", "changedBefore": "2999-01-01T00:00:00+09:00" });
        assert_eq!(listed(&root, item), vec!["new.txt", "old.txt"]);
        // 오프셋 없는 날짜 로컬 시간 표식
        let naive = run_find(&root, json!({ "changedWithin": "2000-01-01" }));
        assert!(result_text(&naive).contains("read as local time (UTC"));
        let explicit = run_find(&root, json!({ "changedWithin": "2000-01-01T00:00:00Z" }));
        assert!(!result_text(&explicit).contains("local time"));
        for bad in [
            "3 months",
            "1y",
            "",
            "10",
            "2026-13-01",
            "2023-02-29",
            "2026-11-01T25:00",
        ] {
            let result = run_find(&root, json!({ "changedWithin": bad }));
            assert!(result.is_error, "{bad}");
            assert!(
                result_text(&result).contains("Invalid changedWithin"),
                "{bad}"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn civil_dates_round_trip_through_epoch() {
        let parse = |text: &str| parse_civil_seconds(text).unwrap();
        assert_eq!(parse("1970-01-01T00:00:00Z"), (0, false));
        assert_eq!(parse("2000-03-01"), (951_868_800, true));
        assert_eq!(parse("2024-02-29 12:30:15"), (1_709_209_815, true));
        assert_eq!(parse("1969-12-31T23:59:59.250Z"), (-1, false));
        assert_eq!(
            parse("2026-11-09T12:00:00+09:00"),
            parse("2026-11-09T03:00:00Z")
        );
        assert_eq!(
            parse("2026-11-09T12:00-0130").0,
            parse("2026-11-09T13:30Z").0
        );
        assert!(parse_civil_seconds("2026-11-09T12:00:00+9").is_none());
        let point = UNIX_EPOCH + Duration::from_secs(parse("2026-11-09T03:04:05Z").0 as u64);
        assert_eq!(format_rfc3339_utc(point), "2026-11-09T03:04:05Z");
        assert_eq!(
            format_rfc3339_utc(UNIX_EPOCH - Duration::from_millis(500)),
            "1969-12-31T23:59:59Z"
        );
    }
    #[test]
    fn max_and_min_depth_bound_levels() {
        let root = sample_tree("rfm-find-depth");
        assert_eq!(
            listed(&root, json!({ "maxDepth": 1 })),
            vec!["Main.rs", "main.txt", "notes.TOML", "src/"]
        );
        assert_eq!(
            listed(&root, json!({ "minDepth": 3 })),
            vec!["src/deep/a.rs", "src/deep/b.md"]
        );
        assert!(listed(&root, json!({ "maxDepth": 0 })).is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn dot_patterns_include_hidden_entries() {
        let root = sample_tree("rfm-find-hidden");
        write_file(&root, ".env", b"KEY=1\n");
        write_file(&root, ".config/app.toml", b"a = 1\n");
        assert!(
            !listed(&root, json!({}))
                .iter()
                .any(|line| line.starts_with('.'))
        );
        assert_eq!(listed(&root, json!({ "glob": ".env*" })), vec![".env"]);
        assert_eq!(
            listed(&root, json!({ "pattern": "^\\.c" })),
            vec![".config/"]
        );
        assert_eq!(
            listed(&root, json!({ "glob": ".config/*", "fullPath": true })),
            vec![".config/app.toml"]
        );
        assert!(listed(&root, json!({ "glob": ".env", "includeHidden": false })).is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn gitignore_respected_inside_repo_and_disabled() {
        let root = temp_dir("rfm-find-gitignore");
        fs::create_dir_all(root.join(".git")).unwrap();
        write_file(&root, ".gitignore", b"*.log\nbuild/\n");
        write_file(&root, "keep.rs", b"\n");
        write_file(&root, "drop.log", b"\n");
        write_file(&root, "build/out.rs", b"\n");
        assert_eq!(listed(&root, json!({})), vec!["keep.rs"]);
        assert_eq!(
            listed(&root, json!({ "respectGitignore": false })),
            vec!["build/", "build/out.rs", "drop.log", "keep.rs"]
        );
        // hidden 포함 시 .git 기본 제외
        assert_eq!(
            listed(&root, json!({ "includeHidden": true })),
            vec![".gitignore", "keep.rs"]
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn exclude_patterns_and_default_excludes() {
        let root = sample_tree("rfm-find-exclude");
        write_file(&root, "node_modules/pkg/index.js", b"\n");
        write_file(&root, "target/debug/app", b"\n");
        let item = json!({ "excludePatterns": ["*.md", "deep/"], "type": "file" });
        assert_eq!(
            listed(&root, item),
            vec!["Main.rs", "main.txt", "notes.TOML", "src/lib.rs"]
        );
        assert!(
            !listed(&root, json!({}))
                .iter()
                .any(|line| line.starts_with("node_modules"))
        );
        let all = listed(&root, json!({ "noDefaultExcludes": true, "type": "file" }));
        assert!(
            all.contains(&"node_modules/pkg/index.js".to_string()),
            "{all:?}"
        );
        assert!(all.contains(&"target/debug/app".to_string()), "{all:?}");
        // 루트가 heavy dir 내부면 기본 제외 해제
        assert_eq!(
            listed(&root.join("node_modules"), json!({ "type": "file" })),
            vec!["pkg/index.js"]
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn sort_by_modified_and_size() {
        let root = temp_dir("rfm-find-sort");
        write_file(&root, "a.bin", &[0; 300]);
        write_file(&root, "b.bin", &[0; 100]);
        write_file(&root, "c.bin", &[0; 200]);
        let now = SystemTime::now();
        set_mtime(&root.join("a.bin"), now - Duration::from_secs(3_000));
        set_mtime(&root.join("b.bin"), now - Duration::from_secs(1_000));
        set_mtime(&root.join("c.bin"), now - Duration::from_secs(2_000));
        assert_eq!(
            listed(&root, json!({ "sortBy": "modified" })),
            vec!["b.bin", "c.bin", "a.bin"]
        );
        assert_eq!(
            listed(&root, json!({ "sortBy": "size" })),
            vec!["a.bin", "c.bin", "b.bin"]
        );
        // 상위 N 은 전체 정렬의 앞부분
        assert_eq!(
            listed(&root, json!({ "sortBy": "size", "maxResults": 1 })),
            vec!["a.bin"]
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn details_append_size_and_utc_mtime() {
        let root = temp_dir("rfm-find-details");
        write_file(&root, "dir/file.txt", b"12345");
        let stamp = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        set_mtime(&root.join("dir/file.txt"), stamp);
        let lines = listed(&root, json!({ "details": true }));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("dir/\t"), "{lines:?}");
        assert_eq!(lines[0].matches('\t').count(), 1);
        assert_eq!(lines[1], "dir/file.txt\t5 B\t2023-11-14T22:13:20Z");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn max_results_returns_exact_first_page() {
        let root = temp_dir("rfm-find-limit");
        // 병렬 워커 경로 (프로브 상한 256 초과)
        for group in 0..12 {
            for index in 0..30 {
                write_file(&root, &format!("g{group:02}/f{index:02}.txt"), b"x");
            }
        }
        let full = listed(&root, json!({ "type": "file" }));
        assert_eq!(full.len(), 360);
        assert_eq!(full, sorted(full.clone()));
        for cap in [1, 7, 45] {
            let result = run_find(&root, json!({ "type": "file", "maxResults": cap }));
            let structured = result.structured.clone().unwrap();
            assert_eq!(structured["limitReached"], json!(true));
            assert_eq!(structured["totalCount"], json!(cap));
            let page = listed(&root, json!({ "type": "file", "maxResults": cap }));
            assert_eq!(page, full[..cap].to_vec());
        }
        let text = result_text(&run_find(&root, json!({ "maxResults": 2 })));
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(format!("{}:", root.display()).as_str()));
        assert!(lines.next().unwrap().starts_with("[stopped at maxResults"));
        fs::remove_dir_all(root).unwrap();
    }
    fn entry_of(rel: &str) -> FoundEntry {
        FoundEntry {
            rank: 0,
            rel: rel.to_string(),
            tail: String::new(),
        }
    }
    fn rels_of(set: TopSet) -> Vec<String> {
        set.heap
            .into_sorted_vec()
            .into_iter()
            .map(|found| found.rel)
            .collect()
    }
    #[test]
    fn byte_budget_cut_is_prefix_closed() {
        // 긴 항목 축출 후 짧은 후순 항목
        let long = format!("b{}", "x".repeat(200));
        let mut set = TopSet::default();
        for rel in ["a", long.as_str(), "c"] {
            keep_top_entry(&mut set, entry_of(rel), usize::MAX, 10);
        }
        assert!(set.clipped);
        assert_eq!(rels_of(set), vec!["a"]);
    }
    #[test]
    fn merge_applies_lowest_worker_floor() {
        // 워커 A 축출 키보다 뒤인 워커 B 항목
        let long = format!("b{}", "x".repeat(200));
        let mut first = TopSet::default();
        for rel in ["a", long.as_str()] {
            keep_top_entry(&mut first, entry_of(rel), usize::MAX, 10);
        }
        let mut second = TopSet::default();
        keep_top_entry(&mut second, entry_of("c"), usize::MAX, 10);
        for sets in [
            vec![first, second],
            vec![TopSet::default(), TopSet::default()],
        ] {
            let had_floor = sets.iter().any(|set| set.floor.is_some());
            let merged = merge_top_sets(sets, usize::MAX, 10);
            if had_floor {
                assert!(merged.clipped);
                assert_eq!(rels_of(merged), vec!["a"]);
            } else {
                assert!(!merged.clipped);
                assert!(merged.heap.is_empty());
            }
        }
    }
    #[test]
    fn byte_budget_walk_lists_exact_prefix() {
        let root = temp_dir("rfm-find-budget");
        // 병렬 워커 경로, 긴 이름·짧은 이름 교차
        for group in 0..12 {
            for index in 0..30 {
                let pad = if index % 3 == 0 {
                    "x".repeat(120)
                } else {
                    String::new()
                };
                write_file(
                    &root,
                    &format!("g{group:02}/f{index:02}{pad}.txt"),
                    &vec![b'x'; index],
                );
            }
        }
        for sort in ["path", "size"] {
            let item = json!({ "path": root.display().to_string(), "sortBy": sort });
            let mut spec = read_find_spec(&item).unwrap();
            let full = run_find_walk(&spec).unwrap();
            assert!(!full.limit_reached);
            let cost = |found: &FoundEntry| found.rel.len() + found.tail.len() + 1;
            for budget in [300, 1_000, 4_000] {
                spec.byte_budget = budget;
                let mut used = 0;
                let expected: Vec<&str> = full
                    .entries
                    .iter()
                    .take_while(|found| {
                        used += cost(found);
                        used <= budget
                    })
                    .map(|found| found.rel.as_str())
                    .collect();
                for _ in 0..5 {
                    let page = run_find_walk(&spec).unwrap();
                    assert!(page.limit_reached, "{sort} {budget}");
                    let got: Vec<&str> = page
                        .entries
                        .iter()
                        .map(|found| found.rel.as_str())
                        .collect();
                    assert_eq!(got, expected, "{sort} {budget}");
                }
            }
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn extreme_numeric_arguments_never_panic() {
        let root = sample_tree("rfm-find-extreme");
        let huge = json!(u64::MAX);
        for item in [
            json!({ "maxResults": huge }),
            json!({ "maxDepth": huge }),
            json!({ "minDepth": huge }),
            json!({ "timeout_ms": huge }),
        ] {
            let result = run_find(&root, item.clone());
            assert!(!result.is_error, "{item} {}", result_text(&result));
        }
        assert!(listed(&root, json!({ "minDepth": huge })).is_empty());
        assert_eq!(listed(&root, json!({ "maxResults": huge })).len(), 8);
        for size in ["+99999999999t", "18446744073709551616b", "-17000000ti"] {
            let result = run_find(&root, json!({ "size": size }));
            assert!(result_text(&result).contains("Invalid size"), "{size}");
        }
        for when in [
            "999999999999w",
            "18446744073709551615s",
            "30000000000000000s",
            "1w 18446744073709551615s",
            "@9223372036854775807",
            "@-9223372036854775808",
            "@99999999999999999999",
        ] {
            let result = run_find(&root, json!({ "changedWithin": when }));
            assert!(result.is_error, "{when}");
            assert!(
                result_text(&result).contains("Invalid changedWithin"),
                "{when}"
            );
        }
        // 범위 끝 날짜 (플랫폼별 표현 한계)
        for when in [
            "0000-01-01",
            "9999-12-31T23:59:59-23:59",
            "0001-03-01T00:00:00+23:59",
        ] {
            let result = run_find(&root, json!({ "changedBefore": when }));
            let text = result_text(&result);
            assert!(
                !result.is_error || text.contains("Invalid changedBefore"),
                "{when} {text}"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn zero_timeout_flags_partial_or_errors() {
        let root = temp_dir("rfm-find-timeout");
        for index in 0..300 {
            write_file(&root, &format!("d{}/f{index:03}.txt", index % 10), b"x");
        }
        let result = run_find(&root, json!({ "timeout_ms": 0 }));
        let text = result_text(&result);
        if result.is_error {
            assert!(text.contains("timed out"), "{text}");
        } else {
            assert_eq!(result.structured.unwrap()["timedOut"], json!(true));
            assert!(text.contains("[walk stopped at the time budget"), "{text}");
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn missing_or_file_root_and_no_matches() {
        let missing = std::env::temp_dir().join("rfm-find-definitely-missing-root");
        let result = run_find(&missing, json!({}));
        assert!(result_text(&result).contains("Path does not exist"));
        let root = sample_tree("rfm-find-errors");
        let file_root = run_find(&root.join("Main.rs"), json!({}));
        assert!(file_root.is_error);
        assert!(result_text(&file_root).contains("path-stat"));
        let none = run_find(&root, json!({ "glob": "*.zzz" }));
        assert_eq!(
            result_text(&none),
            format!("{}:\n(no matches)", root.display())
        );
        assert_eq!(none.structured.unwrap()["totalCount"], json!(0));
        let batch = handle_path_find(&json!({ "path": root.display().to_string() }));
        assert!(batch.is_error);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn matches_fd_file_sets() {
        if std::process::Command::new("fd")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let root = sample_tree("rfm-find-fd");
        write_file(&root, "src/deep/nested/c.rs", b"\n");
        write_file(&root, "docs/Guide.MD", b"\n");
        write_file(&root, ".hidden/x.rs", b"\n");
        let fd_lines = |args: &[&str]| -> Vec<String> {
            let output = std::process::Command::new("fd")
                .args(["--no-ignore", "--color", "never", "--path-separator", "/"])
                .arg("--base-directory")
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
            let text = String::from_utf8(output.stdout).unwrap();
            sorted(
                text.lines()
                    .map(|line| line.trim_start_matches("./").to_string())
                    .collect(),
            )
        };
        let cases = [
            (
                vec!["-t", "f", "-e", "rs"],
                json!({ "type": "file", "extension": "rs" }),
            ),
            (vec!["-t", "d"], json!({ "type": "directory" })),
            (vec!["-g", "*.md"], json!({ "glob": "*.md" })),
            (vec!["^[a-m]"], json!({ "pattern": "^[a-m]" })),
            (
                vec!["-H", "-t", "f", "-d", "2"],
                json!({ "includeHidden": true, "type": "file", "maxDepth": 2 }),
            ),
            (
                vec!["-t", "f", "-S", "-6b"],
                json!({ "type": "file", "size": "-6b" }),
            ),
        ];
        for (fd_args, ours) in cases {
            let mut ours = ours;
            ours["respectGitignore"] = json!(false);
            assert_eq!(
                sorted(listed(&root, ours)),
                fd_lines(&fd_args),
                "{fd_args:?}"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn offset_free_dates_match_fd_local_time() {
        if std::process::Command::new("fd")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        // 로컬 자정·시각 경계 걸친 15분 격자 (±15h, ±1초)
        let root = temp_dir("rfm-find-local-date");
        let values = [
            "2026-11-09",
            "2026-11-09 12:34:56",
            "1950-07-01",
            "1950-07-01 00:30:00",
        ];
        for (value_index, value) in values.iter().enumerate() {
            let (naive, _) = parse_civil_seconds(value).unwrap();
            for step in -60_i64..=60 {
                for nudge in [-1_i64, 1] {
                    let rel = format!("v{value_index}_{:03}_{}.txt", step + 60, nudge + 1);
                    write_file(&root, &rel, b"");
                    let seconds = naive + step * 900 + nudge;
                    let time = if seconds >= 0 {
                        UNIX_EPOCH + Duration::from_secs(seconds.unsigned_abs())
                    } else {
                        UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
                    };
                    set_mtime(&root.join(rel), time);
                }
            }
        }
        for value in values {
            for (field, flag) in [
                ("changedWithin", "--changed-within"),
                ("changedBefore", "--changed-before"),
            ] {
                let output = std::process::Command::new("fd")
                    .args(["--no-ignore", "--color", "never", "-t", "f", flag, value])
                    .arg("--base-directory")
                    .arg(&root)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "fd {flag} {value}");
                let fd_set = sorted(
                    String::from_utf8(output.stdout)
                        .unwrap()
                        .lines()
                        .map(|line| line.trim_start_matches("./").to_string())
                        .collect(),
                );
                let item = json!({ "type": "file", "respectGitignore": false, field: value });
                let ours = sorted(listed(&root, item));
                let only_ours: Vec<&String> = ours.iter().filter(|n| !fd_set.contains(n)).collect();
                let only_fd: Vec<&String> = fd_set.iter().filter(|n| !ours.contains(n)).collect();
                assert!(
                    only_ours.is_empty() && only_fd.is_empty(),
                    "{field} {value}: only ours {only_ours:?}, only fd {only_fd:?}"
                );
            }
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn tz_values_match_fd_local_time() {
        if std::process::Command::new("fd")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        // 공백·겹침·남반구·역사 값 격자 (TZ 는 fd 자식에만 설정)
        let root = temp_dir("rfm-find-tz");
        let values = [
            "2026-03-08 02:30:00",
            "2026-11-01 01:30:00",
            "2026-07-01",
            "2026-04-05 02:30:00",
            "2026-10-04 02:30:00",
            "1950-07-01",
            "1970-07-01",
            "2026-03-29 02:30:00",
            "2026-10-25 02:30:00",
        ];
        let mut grid: Vec<(String, i64)> = Vec::new();
        for (value_index, value) in values.iter().enumerate() {
            let (naive, _) = parse_civil_seconds(value).unwrap();
            for step in -60_i64..=60 {
                for nudge in [-1_i64, 1] {
                    let rel = format!("v{value_index}_{:03}_{}.txt", step + 60, nudge + 1);
                    write_file(&root, &rel, b"");
                    let seconds = naive + step * 900 + nudge;
                    let time = if seconds >= 0 {
                        UNIX_EPOCH + Duration::from_secs(seconds.unsigned_abs())
                    } else {
                        UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
                    };
                    set_mtime(&root.join(&rel), time);
                    grid.push((rel, seconds));
                }
            }
        }
        // TZif 절대 경로 (정상·잘린 파일)
        let tzif_dir = temp_dir("rfm-find-tzif");
        let tzif = sample_tzif();
        fs::write(tzif_dir.join("Sample"), &tzif).unwrap();
        fs::write(tzif_dir.join("Cut"), &tzif[..tzif.len() - 9]).unwrap();
        let tzif_path = tzif_dir.join("Sample").display().to_string();
        let cut_path = tzif_dir.join("Cut").display().to_string();
        let colon_path = format!(":{tzif_path}");
        let zones = [
            tzif_path.as_str(),
            colon_path.as_str(),
            cut_path.as_str(),
            "CST6CDT",
            "MST7MDT",
            "PST8PDT",
            "MST",
            "HST",
            "CET",
            "EET",
            "MET",
            "WET",
            "cet",
            "Europe/Kyiv",
            "America/Ciudad_Juarez",
            "America/New_York",
            ":America/New_York",
            "EST5EDT,M3.2.0,M11.1.0",
            "AEST-10AEDT,M10.1.0,M4.1.0/3",
            "Australia/Sydney",
            "<+0330>-3:30",
            "ABC5DEF,J182/0,J183",
            "ABC5DEF,181/0,182/0",
            "ABC5DEF",
            "garbage",
            "",
            "JST-9",
            "Asia/Tokyo",
            "GMT+9",
            "JST",
            "Etc/GMT-3",
            "UTC",
            "asia/tokyo",
            "AMERICA/NEW_YORK",
            "US/Eastern",
            "Japan",
            "EST",
            "EST5EDT",
            "UTC0",
            "<+09>-9",
            " Asia/Tokyo",
            "Asia/Tokyo ",
            "C:/nonexistent/zone",
            "/etc/localtime",
            "Etc/Unknown",
            "Factory",
            "posixrules",
            "EST5",
            "ABC5DEF,M3.2.0/-1,M11.1.0/26",
            "UTC+3",
            "Asia/Seoul",
            "ROK",
            "SystemV/EST5",
            "ACT",
        ];
        let mut mismatches = Vec::new();
        for zone in zones {
            for (value_index, value) in values.iter().enumerate() {
                let prefix = format!("v{value_index}_");
                let (naive, _) = parse_civil_seconds(value).unwrap();
                let tz = std::ffi::OsStr::new(zone);
                let (cutoff, _) =
                    local_zone::resolve_with_tz(naive, Some(tz), local_zone::load_icu_api())
                        .unwrap();
                let ours = sorted(
                    grid.iter()
                        .filter(|(rel, seconds)| rel.starts_with(&prefix) && *seconds < cutoff)
                        .map(|(rel, _)| rel.clone())
                        .collect(),
                );
                let output = std::process::Command::new("fd")
                    .args(["--no-ignore", "--color", "never", "-t", "f"])
                    .args(["--changed-before", value, "--base-directory"])
                    .arg(&root)
                    .env("TZ", zone)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "fd TZ={zone} {value}");
                let fd_set: Vec<String> = sorted(
                    String::from_utf8(output.stdout)
                        .unwrap()
                        .lines()
                        .map(|line| line.trim_start_matches("./").to_string())
                        .filter(|line| line.starts_with(&prefix))
                        .collect(),
                );
                if ours != fd_set {
                    // fd 컷오프 = 마지막 포함 파일 다음 격자
                    let fd_cutoff = grid
                        .iter()
                        .filter(|(rel, _)| fd_set.last() == Some(rel))
                        .map(|(_, seconds)| seconds + 1)
                        .next();
                    mismatches.push(format!(
                        "TZ={zone:?} {value}: ours offset {}, fd offset {:?}",
                        naive - cutoff,
                        fd_cutoff.map(|fd_cutoff| naive - fd_cutoff)
                    ));
                }
            }
        }
        assert!(mismatches.is_empty(), "{mismatches:#?}");
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(tzif_dir).unwrap();
    }
    // v2 TZif 표본 (EU 규칙 바닥글)
    #[cfg(windows)]
    pub(super) fn sample_tzif() -> Vec<u8> {
        let header = |bytes: &mut Vec<u8>| {
            bytes.extend_from_slice(b"TZif2");
            bytes.extend_from_slice(&[0; 15]);
            for count in [0_u32, 0, 0, 3, 3, 12] {
                bytes.extend_from_slice(&count.to_be_bytes());
            }
        };
        let transitions = [-315_619_200_i64, 0, 31_536_000];
        let tail = |bytes: &mut Vec<u8>| {
            bytes.extend_from_slice(&[1, 2, 1]);
            for (offset, dst, name) in [(1_800_i32, 0, 0), (3_600, 0, 4), (7_200, 1, 8)] {
                bytes.extend_from_slice(&offset.to_be_bytes());
                bytes.extend_from_slice(&[dst, name]);
            }
            bytes.extend_from_slice(b"LMT\0AAA\0BBB\0");
        };
        let mut bytes = Vec::new();
        header(&mut bytes);
        for at in transitions {
            bytes.extend_from_slice(&i32::try_from(at).unwrap().to_be_bytes());
        }
        tail(&mut bytes);
        header(&mut bytes);
        for at in transitions {
            bytes.extend_from_slice(&at.to_be_bytes());
        }
        tail(&mut bytes);
        bytes.extend_from_slice(b"\nAAA-1BBB,M3.5.0,M10.5.0/3\n");
        bytes
    }
    #[test]
    fn local_instants_follow_fd_later_disambiguation() {
        // 가짜 시간대: 1000 이전 -5h, 이후 -4h (공백) / 역전이 (겹침)
        let transition = 1_000_000;
        let mut spring = |seconds: i64| {
            Some(if seconds < transition {
                -18_000
            } else {
                -14_400
            })
        };
        let mut autumn = |seconds: i64| {
            Some(if seconds < transition {
                -14_400
            } else {
                -18_000
            })
        };
        // 평상 시각: 유일 후보
        assert_eq!(
            pick_local_instant(transition - 18_000 - 7_200, &mut spring),
            Some((transition - 7_200, -18_000))
        );
        assert_eq!(
            pick_local_instant(transition - 14_400 + 7_200, &mut spring),
            Some((transition + 7_200, -14_400))
        );
        // 공백 30분 지점: 전이 전 오프셋, 시각 전진
        let gap_local = transition - 18_000 + 1_800;
        assert_eq!(
            pick_local_instant(gap_local, &mut spring),
            Some((transition + 1_800, -18_000))
        );
        // 겹침 30분 지점: 늦은 시각 (fd 실측)
        let fold_local = transition - 14_400 - 1_800;
        assert_eq!(
            pick_local_instant(fold_local, &mut autumn),
            Some((transition + 1_800, -18_000))
        );
        // 탐침 실패·범위 끝
        assert_eq!(pick_local_instant(0, &mut |_| None), None);
        assert_eq!(pick_local_instant(i64::MAX, &mut |_| Some(0)), None);
    }
    #[test]
    fn local_dates_round_trip_through_system_zone() {
        // 날짜만: 로컬 자정, 시각 포함: 그 벽시계
        for text in ["2026-11-09", "2026-11-09 12:34:56", "2026-07-01T23:59:59"] {
            let (naive, _) = parse_civil_seconds(text).unwrap();
            let (utc, offset) = local_zone::local_to_utc_seconds(naive).unwrap();
            assert_eq!(utc + offset, naive, "{text}");
            assert!(offset.abs() <= 26 * 3_600, "{text} {offset}");
            let (point, local_offset) = parse_time_point("changedWithin", text).unwrap();
            assert_eq!(local_offset, Some(offset));
            assert_eq!(point, UNIX_EPOCH + Duration::from_secs(utc.unsigned_abs()));
        }
        // 오프셋·epoch 값은 정확 유지
        let (point, local_offset) =
            parse_time_point("changedWithin", "2026-11-09T12:00:00+09:00").unwrap();
        assert_eq!(local_offset, None);
        assert_eq!(point, UNIX_EPOCH + Duration::from_secs(1_794_193_200));
        let (point, local_offset) = parse_time_point("changedWithin", "@1794193200").unwrap();
        assert_eq!(
            (point, local_offset),
            (UNIX_EPOCH + Duration::from_secs(1_794_193_200), None)
        );
    }
}
