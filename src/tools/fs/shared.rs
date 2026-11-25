//! shared.rs
//! tools::fs::shared
//!
//! Item readers, mutation touched-path extractors, and line counting shared by the fs handlers.
//!

use serde_json::{Value, json};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Condvar, Mutex, PoisonError};

// 0-1. Path write lock state --------------------------------------------------
// 쓰기 중인 파일의 어휘 키 집합 (보유 수가 작아 Vec 선형 탐색). 호출자는 ensure_path_allowed 로 절대·어휘 정규화한
// 경로를 넘긴다. canonical_key(canonicalize syscall)는 쓰기 1회당 0.1~0.2ms 느려져(2026-11 A/B) 쓰지 않는다.
static PATH_LOCKS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static PATH_LOCKS_FREED: Condvar = Condvar::new();

// 파일 변경 경로 잠금 guard: Drop 에서 키를 반납하고 대기자를 깨운다
pub(super) struct PathLock {
    keys: Vec<String>,
}
impl Drop for PathLock {
    fn drop(&mut self) {
        if self.keys.is_empty() {
            return;
        }
        let mut held = PATH_LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
        held.retain(|key| !self.keys.contains(key));
        drop(held);
        PATH_LOCKS_FREED.notify_all();
    }
}

// 1. Item readers -------------------------------------------------------------
pub(super) fn read_items(args: &Value) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(paths) = args.get("paths").and_then(Value::as_array) {
        items.extend(
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(|path| json!({ "path": path })),
        );
    }
    if let Some(raw_items) = args.get("items").and_then(Value::as_array) {
        items.extend(raw_items.iter().cloned());
    }
    items
}
pub(super) fn usize_field(value: &Value, key: &str, default: usize) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default)
}
// 2. Mutation touched paths ---------------------------------------------------
// Mutation 배치의 독립성 판정에 쓰이는 touched-path 추출기(go pathTouched 계열 대응).
// 필수 키가 없으면 빈 벡터를 돌려줘 순차 폴백을 유도한다.
fn string_field_vec(item: &Value, keys: &[&str], required: &str) -> Vec<String> {
    if item.get(required).and_then(Value::as_str).is_none() {
        return Vec::new();
    }
    keys.iter()
        .filter_map(|key| item.get(*key).and_then(Value::as_str).map(str::to_string))
        .collect()
}
pub(super) fn path_touched(item: &Value) -> Vec<String> {
    string_field_vec(item, &["path"], "path")
}
pub(super) fn write_touched(item: &Value) -> Vec<String> {
    string_field_vec(item, &["path", "content_path"], "path")
}
pub(super) fn transfer_touched(item: &Value) -> Vec<String> {
    let touched = string_field_vec(item, &["source", "destination"], "source");
    if touched.len() < 2 {
        return Vec::new();
    }
    touched
}
pub(super) fn edit_touched(item: &Value) -> Vec<String> {
    string_field_vec(
        item,
        &["file_path", "old_string_path", "new_string_path"],
        "file_path",
    )
}
pub(super) fn edit_lines_touched(item: &Value) -> Vec<String> {
    string_field_vec(item, &["file_path", "replacement_path"], "file_path")
}
// 3. Parent directory and line count ------------------------------------------
pub(super) fn ensure_parent_dir(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create {}: {error}", parent.display()))
}
// 반환 윈도우(슬라이스 내용) 기준 라인 수: 개행 수 + 마지막 줄이 개행으로 끝나지 않으면 +1.
pub(super) fn window_line_count(content: &str) -> usize {
    let breaks = memchr::memchr_iter(b'\n', content.as_bytes()).count();
    breaks + usize::from(!content.is_empty() && !content.ends_with('\n'))
}
// 4. Open-first file access ---------------------------------------------------
pub(super) enum OpenFailure {
    Missing,
    Directory,
    Unreadable(io::Error),
}
// 열기 1회 판정 (stat 재오픈 제거)
pub(super) fn open_regular_file(path: &Path) -> Result<(fs::File, fs::Metadata), OpenFailure> {
    let opened = fs::File::open(path).and_then(|file| {
        let metadata = file.metadata()?;
        Ok((file, metadata))
    });
    match opened {
        Ok((_, metadata)) if metadata.is_dir() => Err(OpenFailure::Directory),
        Ok(opened) => Ok(opened),
        // 열기 실패 분류
        Err(open_error) => match fs::metadata(path) {
            Err(_) => Err(OpenFailure::Missing),
            Ok(metadata) if metadata.is_dir() => Err(OpenFailure::Directory),
            Ok(_) => Err(OpenFailure::Unreadable(open_error)),
        },
    }
}
// 5. Path write locks ---------------------------------------------------------
// 같은 파일의 동시 요청을 읽기부터 쓰기까지 직렬화한다. 요청 키를 한 번에 모두 잡아 보유 중 대기가 없다.
pub(super) fn lock_paths(paths: &[&Path]) -> PathLock {
    let mut keys: Vec<String> = paths
        .iter()
        .map(|path| {
            let key = path.to_string_lossy().replace('\\', "/");
            if cfg!(windows) {
                key.to_ascii_lowercase()
            } else {
                key
            }
        })
        .collect();
    keys.sort();
    keys.dedup();
    if keys.is_empty() {
        return PathLock { keys };
    }
    let mut held = PATH_LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    while keys.iter().any(|key| held.contains(key)) {
        held = PATH_LOCKS_FREED
            .wait(held)
            .unwrap_or_else(PoisonError::into_inner);
    }
    held.extend(keys.iter().cloned());
    PathLock { keys }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::fs::make_temp_dir;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn lock_paths_serializes_same_path() {
        let dir = make_temp_dir("rust-fs-mcp-lock-same");
        let path = dir.join("shared.txt");
        let inside = AtomicUsize::new(0);
        thread::scope(|scope| {
            for _ in 0..8 {
                let path = path.as_path();
                let inside = &inside;
                scope.spawn(move || {
                    let _guard = lock_paths(&[path]);
                    assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0);
                    thread::sleep(Duration::from_millis(2));
                    inside.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn lock_paths_distinct_paths_do_not_block() {
        let dir = make_temp_dir("rust-fs-mcp-lock-distinct");
        let first = dir.join("a.txt");
        let second = dir.join("b.txt");
        let held = lock_paths(&[first.as_path()]);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _guard = lock_paths(&[second.as_path()]);
            sender.send(()).unwrap();
        });
        assert!(receiver.recv_timeout(Duration::from_secs(5)).is_ok());
        worker.join().unwrap();
        drop(held);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn lock_paths_empty_returns() {
        let guard = lock_paths(&[]);
        assert!(guard.keys.is_empty());
    }
}
