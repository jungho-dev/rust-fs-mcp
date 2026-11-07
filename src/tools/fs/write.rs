//! write.rs
//! tools::fs::write
//!
//! file-write handler plus the atomic temp-file + rename writer shared with the edit tools.
//!

use super::shared::{ensure_parent_dir, write_touched};
use crate::core::batch::{create_batch_response, run_batch_mutation};
use crate::core::config::target_path;
use crate::core::response::RawResult;
use crate::tools::common::item_content;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::UNIX_EPOCH;

// 1. File write tool ----------------------------------------------------------
pub fn handle_file_write(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    // 독립 경로만 건드리는 mutation 배치는 병렬(충돌·조상 관계는 순차 폴백).
    let results = run_batch_mutation(items, write_touched, write_item);
    create_batch_response("file-write", results, false)
}
fn write_item(item: &Value) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let content = match item_content(item, "content", "content_path") {
        Ok(content) => content,
        Err(error) => return RawResult::error(error),
    };
    let path = match target_path(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if let Err(error) = ensure_parent_dir(&path) {
        return RawResult::error(error);
    }
    let mode = item
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("rewrite");
    let result = if mode == "append" {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(content.as_bytes()))
    } else {
        write_atomic(&path, content.as_bytes())
    };

    match result {
        Ok(()) => RawResult::structured(
            format!("Wrote {} bytes to {}", content.len(), path.display()),
            json!({
                "path": path.display().to_string(),
                "bytes": content.len(),
                "mode": mode
            }),
        ),
        Err(error) => RawResult::error(format!("Failed to write {}: {error}", path.display())),
    }
}
// 2. Atomic writer ------------------------------------------------------------
// Rewrite-mode writes go through a same-directory temp file + rename so an interrupted
// write never leaves the destination truncated. rename replaces an existing file on both
// Windows (MOVEFILE_REPLACE_EXISTING) and POSIX. When the temp file cannot be created or
// the final rename fails (e.g. exotic permissions), fall back to the direct write so the
// success envelope stays identical to the previous behavior.
pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // lstat 1회로 심링크 여부와 기존 권한을 함께 얻는다(심링크가 아니면 lstat 결과가 곧 stat
    // 결과라 두 번째 stat을 없앤다: 이 호스트 stat 1회 ~130µs).
    let link_meta = fs::symlink_metadata(path).ok();
    let is_link = link_meta
        .as_ref()
        .is_some_and(|meta| meta.file_type().is_symlink());
    let resolved;
    let (path, existing_permissions) = if is_link {
        match fs::canonicalize(path) {
            Ok(target) => {
                resolved = target;
                // 심링크는 링크가 아니라 대상의 권한을 승계한다.
                (
                    resolved.as_path(),
                    fs::metadata(&resolved).ok().map(|meta| meta.permissions()),
                )
            }
            // 끊긴 링크: 직접 쓰기
            Err(_) => return fs::write(path, bytes),
        }
    } else {
        (path, link_meta.map(|meta| meta.permissions()))
    };
    // 기존 권한 비트 승계 (읽기 전용 제외)
    let existing_permissions = existing_permissions.filter(|permissions| !permissions.readonly());
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return fs::write(path, bytes);
    };
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let stamp = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let mut temp = None;
    for attempt in 0u32..16 {
        let candidate = parent.join(format!(".{name}.{stamp}-{attempt}.fsmcp.tmp"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temp = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => break,
        }
    }
    let Some((temp_path, mut file)) = temp else {
        return fs::write(path, bytes);
    };
    let written = file.write_all(bytes).and_then(|_| file.flush());
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Some(permissions) = existing_permissions {
        let _ = fs::set_permissions(&temp_path, permissions);
    }
    match fs::rename(&temp_path, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = fs::remove_file(&temp_path);
            fs::write(path, bytes)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::fs::{handle_file_read, make_temp_dir};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn atomic_write_leaves_no_temp_files() {
        let dir = make_temp_dir("rust-fs-mcp-atomic-write");
        let path = dir.join("sample.txt");
        std::fs::write(&path, "old").unwrap();
        let result = handle_file_write(&json!({
            "items": [{ "path": path.display().to_string(), "content": "new content" }]
        }));
        assert!(!result.is_error, "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn atomic_write_through_symlink_keeps_the_link() {
        let dir = make_temp_dir("rust-fs-mcp-symlink-write");
        let target = dir.join("real.txt");
        let link = dir.join("link.txt");
        std::fs::write(&target, "old").unwrap();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&target, &link);
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&target, &link);
        // 심링크 생성 불가 환경 건너뜀
        if linked.is_err() {
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }
        write_atomic(&link, b"new").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn writes_and_reads_file() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-test-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));

        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt");
        let write = handle_file_write(&json!({
            "items": [{ "path": path.display().to_string(), "content": "abc" }]
        }));
        assert!(!write.is_error, "{write:?}");

        let read = handle_file_read(&json!({ "paths": [path.display().to_string()] }));
        assert!(!read.is_error);
        fs::remove_dir_all(&dir).unwrap();
    }
}
