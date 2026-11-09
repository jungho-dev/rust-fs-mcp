//! walk.rs
//! tools::search::walk
//!
//! Parallel ignore walk, per-file grep sink, line clipping, and the match / byte / time budgets.
//!

use super::matcher::SearchMatcher;
use crate::core::batch::available_parallelism;
use crate::core::encoding::decode_line;
use grep_matcher::Matcher;
use grep_searcher::{
    BinaryDetection, Encoding, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch,
};
use ignore::WalkState;
use ignore::overrides::OverrideBuilder;
use std::borrow::Cow;
use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// 1. Search spec and outcome --------------------------------------------------
pub(super) struct SearchSpec {
    pub(super) root: PathBuf,
    pub(super) include_hidden: bool,
    pub(super) respect_ignore: bool,
    pub(super) file_globs: Vec<String>,
    pub(super) default_excludes: bool,
    pub(super) context: usize,
    pub(super) max_results: usize,
    pub(super) max_columns: usize,
    pub(super) byte_budget: usize,
    pub(super) timeout_ms: u64,
    pub(super) multiline: bool,
    pub(super) encoding: Option<Encoding>,
}
pub(super) struct NativeOutcome {
    pub(super) lines: Vec<String>,
    pub(super) timed_out: bool,
    pub(super) partial: bool,
    pub(super) limit_reached: bool,
}
// 파일 단위 그룹 (경로순 정렬)
struct Collected {
    groups: Vec<(PathBuf, Vec<String>)>,
    bytes: usize,
}

// 2. Parallel walk + per-file sink --------------------------------------------
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
    let mut builder = ignore_walk_builder(&spec.root, spec.include_hidden, spec.respect_ignore);
    builder.overrides(overrides);
    Ok(builder)
}
// fs-search·path-find 공통 ignore 규칙
pub(super) fn ignore_walk_builder(
    root: &Path,
    include_hidden: bool,
    respect_ignore: bool,
) -> ignore::WalkBuilder {
    // hidden 스킵, 저장소 .gitignore 만 존중, 심링크 미추적.
    // global excludes·info/exclude·.ignore 제외 (호출당 고정 탐색 비용 약 7ms 실측)
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(!include_hidden)
        .ignore(false)
        .git_ignore(respect_ignore)
        .git_global(false)
        .git_exclude(false)
        .parents(respect_ignore)
        .follow_links(false);
    builder
}
// 워커 기동은 호출당 약 0.5ms/스레드(실측)이고 대형 트리에서만 회수된다. 그래서 루트가
// 파일이면 상한을 프로브하지 않고 바로 단일 스레드로, 디렉터리면 상한까지 걸어 보고 정한다.
// skip_dir: 하위 미집계 디렉터리 이름
pub(super) fn search_threads(root: &Path, skip_dir: Option<impl Fn(&OsStr) -> bool>) -> usize {
    let file_root = std::fs::metadata(root)
        .map(|meta| meta.is_file())
        .unwrap_or(false);
    if file_root {
        return 1;
    }
    if walk_entry_probe(root, skip_dir) >= WALK_PROBE_ENTRIES {
        return available_parallelism().clamp(2, 12);
    }
    1
}
// 디렉터리 엔트리만 세는 규모 추정. gitignore·overrides 필터를 태우지 않아 스캔보다
// 1~2 자릿수 싸고, 상한을 넘으면 즉시 "큰 트리"로 판정한다.
fn walk_entry_probe(root: &Path, skip_dir: Option<impl Fn(&OsStr) -> bool>) -> usize {
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
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
                && !skip_dir
                    .as_ref()
                    .is_some_and(|skip| skip(&entry.file_name()))
            {
                pending.push(entry.path());
            }
        }
    }
    count
}
pub(super) fn run_native_search(
    spec: &SearchSpec,
    matcher: &SearchMatcher,
) -> Result<NativeOutcome, String> {
    // 표현 불가 기한은 무기한
    let deadline = Instant::now().checked_add(Duration::from_millis(spec.timeout_ms));
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
    builder.threads(search_threads(&spec.root, None::<fn(&OsStr) -> bool>));
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
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
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
// 3. Reader cancellation and line sink ----------------------------------------
// Search read cancellation
struct SearchReader<'a, R> {
    reader: R,
    deadline: Option<Instant>,
    timed_out: &'a AtomicBool,
    done: &'a AtomicBool,
}
impl<R: Read> Read for SearchReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.done.load(Ordering::Relaxed) {
            return Ok(0);
        }
        if self.timed_out.load(Ordering::Relaxed)
            || self.deadline.is_some_and(|limit| Instant::now() >= limit)
        {
            self.timed_out.store(true, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "search time budget exceeded",
            ));
        }
        let chunk_len = buffer.len().min(64 * 1024);
        let read_bytes = self.reader.read(&mut buffer[..chunk_len])?;
        if self.deadline.is_some_and(|limit| Instant::now() >= limit) {
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
    deadline: Option<Instant>,
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
        if self.tick & 63 == 0 && self.deadline.is_some_and(|limit| Instant::now() >= limit) {
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

#[cfg(test)]
mod tests {
    use super::super::DEFAULT_MAX_COLUMNS;
    use super::super::matcher::RegexAdapter;
    use super::*;

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
        let deadline = Instant::now().checked_add(Duration::from_millis(5));
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
    fn unrepresentable_deadline_searches_without_time_limit() {
        let matcher = SearchMatcher::Linear(RegexAdapter {
            regex: regex::bytes::Regex::new("needle").unwrap(),
        });
        let timed_out = AtomicBool::new(false);
        let done = AtomicBool::new(false);
        let deadline = Instant::now().checked_add(Duration::MAX);
        assert!(deadline.is_none());
        let reader = SearchReader {
            reader: io::Cursor::new("hay\n".repeat(200) + "needle\n"),
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
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(&matcher, reader, &mut sink)
            .unwrap();
        assert!(!timed_out.load(Ordering::Relaxed));
        assert_eq!(sink.lines, ["201:needle"]);
    }

    #[test]
    fn stop_reader_after_global_match_cap() {
        let timed_out = AtomicBool::new(false);
        let done = AtomicBool::new(true);
        let mut reader = SearchReader {
            reader: io::Cursor::new(b"needle"),
            deadline: Instant::now().checked_add(Duration::from_secs(1)),
            timed_out: &timed_out,
            done: &done,
        };
        assert_eq!(reader.read(&mut [0u8; 16]).unwrap(), 0);
        assert!(!timed_out.load(Ordering::Relaxed));
    }
}
