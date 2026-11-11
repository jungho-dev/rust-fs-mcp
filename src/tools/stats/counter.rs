//! counter.rs
//! tools::stats::counter
//!
//! Per-day tool call table written to `<stats.dir>/YYYY-MM-DD.md` when config.toml sets `[stats] enabled = true`.
//! The call path only bumps an atomic counter; a background thread merges pending counts into the day file every 2 seconds.
//! protocol::server flushes once more when stdin closes; counts a killed process has not flushed yet are lost.
//! Server processes share one day file through a create-new lock file and add only their own pending counts to the counts on disk.
//!

use crate::core::settings::settings;
use crate::tools::search::find::{civil_from_days, local_zone};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, Once, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// 표 행 순서 (분류, 도구)
const TOOL_ROWS: [(&str, &str); 26] = [
    ("Read", "file-read"),
    ("Read", "file-read-line-range"),
    ("Read", "path-stat"),
    ("Directory", "dir-list"),
    ("Directory", "dir-create"),
    ("Write", "file-write"),
    ("Edit", "file-edit"),
    ("Edit", "file-edit-lines"),
    ("Edit", "file-replace"),
    ("Path", "path-copy"),
    ("Path", "path-move"),
    ("Path", "path-remove"),
    ("Search", "fs-search"),
    ("Search", "path-find"),
    ("Search", "ast-search"),
    ("Compare", "file-diff"),
    ("Git", "git-status"),
    ("Git", "git-diff"),
    ("Git", "git-show"),
    ("Git", "git-add"),
    ("Git", "git-commit"),
    ("Git", "git-amend"),
    ("Web", "web-fetch"),
    ("Web", "web-extract"),
    ("Web", "download-to-file"),
    ("SQL", "mysql-query"),
];
const ROW_COUNT: usize = TOOL_ROWS.len();
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const LOCK_FILE_NAME: &str = ".tool-stats.lock";
const LOCK_RETRY_LIMIT: u32 = 100;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);
// 종료된 프로세스 잠금 회수 기준
const STALE_LOCK_AGE: Duration = Duration::from_secs(30);
const COUNT_HEADER: &str = "Calls";
static PENDING_COUNTS: [AtomicU64; ROW_COUNT] = [const { AtomicU64::new(0) }; ROW_COUNT];
static FLUSHER_START: Once = Once::new();
static FLUSH_GUARD: Mutex<()> = Mutex::new(());
static WRITE_WARNED: AtomicBool = AtomicBool::new(false);

// 1. Call counting ------------------------------------------------------------
pub fn record_tool_call(tool_name: &str) {
    if settings().stats_dir.is_none() {
        return;
    }
    let Some(count) = TOOL_ROWS
        .iter()
        .zip(&PENDING_COUNTS)
        .find_map(|((_, tool), count)| (*tool == tool_name).then_some(count))
    else {
        return;
    };
    count.fetch_add(1, Ordering::Relaxed);
    FLUSHER_START.call_once(|| {
        let _ = thread::Builder::new()
            .name("fs-mcp-stats".to_string())
            .spawn(|| {
                loop {
                    thread::sleep(FLUSH_INTERVAL);
                    flush_tool_stats();
                }
            });
    });
}

// 2. Day file flush -----------------------------------------------------------
pub fn flush_tool_stats() {
    let Some(stats_dir) = settings().stats_dir.as_deref() else {
        return;
    };
    let _flushing = FLUSH_GUARD.lock().unwrap_or_else(PoisonError::into_inner);

    // 1. 대기 카운트 회수
    let pending = PENDING_COUNTS
        .each_ref()
        .map(|count| count.swap(0, Ordering::Relaxed));
    if pending.iter().all(|count| *count == 0) {
        return;
    }

    // 2. 로컬 날짜·시각
    let utc_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0);
    let offset = local_zone::local_to_utc_seconds(utc_now).map_or(0, |(_, offset)| offset);
    let local_now = utc_now + offset;
    let (year, month, day) = civil_from_days(local_now.div_euclid(86_400));
    let second_of_day = local_now.rem_euclid(86_400);
    let day_label = format!("{year:04}-{month:02}-{day:02}");
    let time_label = format!(
        "{:02}:{:02}:{:02}",
        second_of_day / 3_600,
        second_of_day / 60 % 60,
        second_of_day % 60
    );

    // 3. 기록 실패 시 카운트 복구
    if let Err(error) = write_day_file(stats_dir, &day_label, &time_label, &pending) {
        if !WRITE_WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "rust-fs-mcp: tool stats write failed ({}): {error}",
                stats_dir.display()
            );
        }
        for (count, delta) in PENDING_COUNTS.iter().zip(pending) {
            count.fetch_add(delta, Ordering::Relaxed);
        }
    }
}
fn write_day_file(
    stats_dir: &Path,
    day_label: &str,
    time_label: &str,
    pending: &[u64; ROW_COUNT],
) -> io::Result<()> {
    // 1. 프로세스 간 잠금
    fs::create_dir_all(stats_dir)?;
    let _lock = acquire_day_lock(stats_dir)?;

    // 2. 기존 카운트 병합
    let day_path = stats_dir.join(format!("{day_label}.md"));
    let mut counts = match fs::read_to_string(&day_path) {
        Ok(text) => parse_day_counts(&text),
        Err(error) if error.kind() == ErrorKind::NotFound => [0; ROW_COUNT],
        Err(error) => return Err(error),
    };
    for (count, delta) in counts.iter_mut().zip(pending) {
        *count = count.saturating_add(*delta);
    }

    // 3. 임시 파일 교체
    let temp_path = stats_dir.join(format!(".{day_label}.md.tmp"));
    fs::write(&temp_path, render_day_table(day_label, time_label, &counts))?;
    fs::rename(&temp_path, &day_path)
}

// 3. Cross-process lock -------------------------------------------------------
struct DayFileLock {
    path: PathBuf,
}
impl Drop for DayFileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
fn acquire_day_lock(stats_dir: &Path) -> io::Result<DayFileLock> {
    let path = stats_dir.join(LOCK_FILE_NAME);
    for _ in 0..LOCK_RETRY_LIMIT {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(DayFileLock { path }),
            // 삭제 대기 중 잠금 (Windows)
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::AlreadyExists | ErrorKind::PermissionDenied
                ) =>
            {
                let stale = fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > STALE_LOCK_AGE);
                if stale {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        ErrorKind::TimedOut,
        format!("lock busy: {}", path.display()),
    ))
}

// 4. Day table format ---------------------------------------------------------
fn parse_day_counts(text: &str) -> [u64; ROW_COUNT] {
    let mut counts = [0; ROW_COUNT];
    for line in text.lines() {
        let cells: Vec<&str> = line.split('│').map(str::trim).collect();
        let [_, _, _, tool_cell, count_cell, _] = cells.as_slice() else {
            continue;
        };
        let Ok(parsed) = count_cell.parse::<u64>() else {
            continue;
        };
        if let Some(slot) = TOOL_ROWS
            .iter()
            .zip(counts.iter_mut())
            .find_map(|((_, tool), slot)| (tool == tool_cell).then_some(slot))
        {
            *slot = parsed;
        }
    }
    counts
}
fn render_day_table(day_label: &str, time_label: &str, counts: &[u64; ROW_COUNT]) -> String {
    // 1. 열 너비
    let total: u64 = counts.iter().sum();
    let index_width = 4;
    let group_width = TOOL_ROWS
        .iter()
        .map(|(group, _)| group.len())
        .max()
        .unwrap_or(0)
        + 2;
    let tool_width = TOOL_ROWS
        .iter()
        .map(|(_, tool)| tool.len())
        .max()
        .unwrap_or(0)
        + 2;
    let count_width = COUNT_HEADER.len().max(total.to_string().len()) + 2;
    let number_width = count_width - 2;
    let pad = |text: &str, width: usize| {
        let fill = width.saturating_sub(text.len() + 1);
        format!(" {text}{}", " ".repeat(fill))
    };
    let rule = |left: &str, joints: [&str; 3], right: &str| {
        let [first, second, third] = joints;
        format!(
            "{left}{}{first}{}{second}{}{third}{}{right}\n",
            "─".repeat(index_width),
            "─".repeat(group_width),
            "─".repeat(tool_width),
            "─".repeat(count_width)
        )
    };

    // 2. 제목·머리글
    let mut table = format!("```text\n{day_label} Tool Usage (updated {time_label})\n\n");
    table.push_str(&rule("┌", ["┬", "┬", "┬"], "┐"));
    let _ = writeln!(
        table,
        "│{}│{}│{}│{}│",
        pad("#", index_width),
        pad("Group", group_width),
        pad("Tool", tool_width),
        pad(COUNT_HEADER, count_width)
    );
    table.push_str(&rule("├", ["┼", "┼", "┼"], "┤"));

    // 3. 분류별 행
    let mut previous_group: Option<&str> = None;
    for (index, ((group, tool), count)) in TOOL_ROWS.iter().zip(counts).enumerate() {
        if previous_group.is_some_and(|previous| previous != *group) {
            table.push_str(&rule("├", ["┼", "┼", "┼"], "┤"));
        }
        previous_group = Some(*group);
        let _ = writeln!(
            table,
            "│ {:>2} │{}│{}│ {count:>number_width$} │",
            index + 1,
            pad(group, group_width),
            pad(tool, tool_width)
        );
    }

    // 4. 합계
    table.push_str(&rule("├", ["┴", "┴", "┼"], "┤"));
    let label_width = index_width + group_width + tool_width + 2;
    let _ = writeln!(
        table,
        "│{}│ {total:>number_width$} │",
        pad("Total", label_width)
    );
    table.push_str(&rule("└", ["─", "─", "┴"], "┘"));
    table.push_str("```\n");
    table
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::catalog::tool_catalog;

    const SAMPLE_COUNTS: [u64; ROW_COUNT] = [
        42, 9, 3, 5, 0, 2, 11, 4, 0, 0, 0, 1, 27, 8, 0, 0, 6, 5, 2, 1, 1, 0, 1, 0, 0, 0,
    ];

    fn temp_stats_dir(prefix: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        std::env::temp_dir().join(format!("{prefix}-{}-{stamp}", std::process::id()))
    }
    #[test]
    fn rows_cover_every_catalog_tool() {
        // 설정 시에만 노출되는 mysql-query 포함
        let catalog = tool_catalog();
        assert_eq!(catalog.len() + 1, ROW_COUNT);
        for tool in &catalog {
            let name = tool["name"].as_str().unwrap_or_default();
            assert!(TOOL_ROWS.iter().any(|(_, row)| *row == name), "{name}");
        }
        assert!(TOOL_ROWS.iter().any(|(_, row)| *row == "mysql-query"));
    }
    #[test]
    fn renders_the_confirmed_table_shape() {
        let text = render_day_table("2026-11-07", "18:42:10", &SAMPLE_COUNTS);
        let lines: Vec<&str> = text.lines().collect();
        let rule = |left: &str, joints: [&str; 3], right: &str| {
            let [first, second, third] = joints;
            format!(
                "{left}{}{first}{}{second}{}{third}{}{right}",
                "─".repeat(4),
                "─".repeat(11),
                "─".repeat(22),
                "─".repeat(7)
            )
        };
        assert_eq!(lines[0], "```text");
        assert_eq!(lines[1], "2026-11-07 Tool Usage (updated 18:42:10)");
        assert_eq!(lines[3], rule("┌", ["┬", "┬", "┬"], "┐"));
        assert_eq!(
            lines[4],
            "│ #  │ Group     │ Tool                 │ Calls │"
        );
        assert_eq!(
            lines[6],
            "│  1 │ Read      │ file-read            │    42 │"
        );
        assert_eq!(lines[9], rule("├", ["┼", "┼", "┼"], "┤"));
        assert_eq!(
            lines[10],
            "│  4 │ Directory │ dir-list             │     5 │"
        );
        let tail: Vec<&str> = lines.iter().rev().take(4).copied().collect();
        assert_eq!(tail[0], "```");
        assert_eq!(tail[1], rule("└", ["─", "─", "┴"], "┘"));
        assert_eq!(tail[2], format!("│ Total{}│   128 │", " ".repeat(33)));
        assert_eq!(tail[3], rule("├", ["┴", "┴", "┼"], "┤"));
        // 표 선 외 ASCII 전용
        let english_only = text
            .chars()
            .all(|ch| ch.is_ascii() || ('─'..='╿').contains(&ch));
        assert!(english_only, "{text}");
        // 그룹 구분선 9개 + 머리글 구분선
        let separators = lines
            .iter()
            .filter(|line| line.starts_with("├────┼"))
            .count();
        assert_eq!(separators, 10);
        assert_eq!(parse_day_counts(&text), SAMPLE_COUNTS);
    }
    #[test]
    fn widens_the_count_column_for_large_totals() {
        let mut counts = [0; ROW_COUNT];
        counts[0] = 123_456_789;
        let text = render_day_table("2026-11-07", "00:00:00", &counts);
        let widths: Vec<usize> = text
            .lines()
            .filter(|line| line.starts_with(['┌', '│', '├', '└']))
            .map(|line| {
                line.chars()
                    .map(|ch| {
                        if ch.is_ascii() || ('─'..='╿').contains(&ch) {
                            1
                        } else {
                            2
                        }
                    })
                    .sum()
            })
            .collect();
        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]), "{text}");
        assert_eq!(parse_day_counts(&text), counts);
    }
    #[test]
    fn day_file_merges_pending_counts() {
        let dir = temp_stats_dir("fs-mcp-stats");
        write_day_file(&dir, "2026-11-07", "10:00:00", &SAMPLE_COUNTS).unwrap();
        write_day_file(&dir, "2026-11-07", "10:00:02", &SAMPLE_COUNTS).unwrap();
        let text = fs::read_to_string(dir.join("2026-11-07.md")).unwrap();
        assert_eq!(
            parse_day_counts(&text),
            SAMPLE_COUNTS.map(|count| count * 2)
        );
        assert!(text.contains("(updated 10:00:02)"), "{text}");
        assert!(!dir.join(LOCK_FILE_NAME).exists());
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn stale_lock_is_reclaimed_and_busy_lock_times_out() {
        let dir = temp_stats_dir("fs-mcp-stats-lock");
        fs::create_dir_all(&dir).unwrap();
        let held = acquire_day_lock(&dir).unwrap();
        let started = std::time::Instant::now();
        let busy = acquire_day_lock(&dir).err().map(|error| error.kind());
        assert_eq!(busy, Some(ErrorKind::TimedOut));
        assert!(started.elapsed() >= LOCK_RETRY_DELAY * (LOCK_RETRY_LIMIT - 1));
        drop(held);

        // 30초 지난 잠금 파일
        let lock_path = dir.join(LOCK_FILE_NAME);
        let lock_file = fs::File::create(&lock_path).unwrap();
        lock_file
            .set_modified(SystemTime::now() - STALE_LOCK_AGE * 2)
            .unwrap();
        drop(lock_file);
        assert!(acquire_day_lock(&dir).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
