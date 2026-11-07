//! repo.rs
//! tools::git::repo
//!
//! git CLI runner, repository toplevel resolution with its cache, and shared git argument readers.
//!

use crate::core::config::ensure_path_allowed;
use crate::core::external::{ExternalTool, MAX_PIPE_BYTES, run_external};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub(super) const GIT_TIMEOUT_MS: u64 = 120_000;

// 1. git runner ---------------------------------------------------------------
// Run a git command. On success (exit 0) returns stdout; on failure returns an stderr-based error.
pub(super) fn run_git(cwd: &Path, args: &[String]) -> Result<String, String> {
    let output = run_external(ExternalTool::Git, args, Some(cwd), Some(GIT_TIMEOUT_MS))?;
    if output.status_code == Some(0) {
        return Ok(output.stdout);
    }
    // 출력 상한 중단: 수집분 성공 처리
    if output.stdout_capped {
        return Ok(format!(
            "{}\n[git output capped at {} MiB by the server]",
            output.stdout,
            MAX_PIPE_BYTES / (1024 * 1024)
        ));
    }
    let detail = if output.stderr.trim().is_empty() {
        output.stdout.trim().to_string()
    } else {
        output.stderr.trim().to_string()
    };
    Err(format!(
        "git failed (code {:?}): {detail}",
        output.status_code
    ))
}
pub(super) fn git_args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| part.to_string()).collect()
}
// 2. Repository resolution ----------------------------------------------------
// Resolve the repository location from the required path argument.
fn resolve_repo_path(args: &Value) -> Result<PathBuf, String> {
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        let resolved = ensure_path_allowed(path)?;
        // If a file path is supplied use its parent directory as the git invocation cwd.
        if resolved.is_file() {
            return Ok(resolved.parent().map(Path::to_path_buf).unwrap_or(resolved));
        }
        return Ok(resolved);
    }
    Err("path is required".to_string())
}
static TOPLEVEL_CACHE: OnceLock<Mutex<HashMap<PathBuf, PathBuf>>> = OnceLock::new();

fn toplevel_cache() -> &'static Mutex<HashMap<PathBuf, PathBuf>> {
    TOPLEVEL_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}
pub(super) fn open_repo(args: &Value) -> Result<PathBuf, String> {
    let path = resolve_repo_path(args)?;
    // 도구 호출마다 들던 rev-parse 스폰 1회를 절약하는 toplevel 캐시.
    // .git 마커가 사라지면(삭제·이동) 무효화하고 다시 해석한다.
    let cached = toplevel_cache().lock().unwrap().get(&path).cloned();
    if let Some(cached) = cached {
        if cached.join(".git").exists() {
            return Ok(cached);
        }
        toplevel_cache().lock().unwrap().remove(&path);
    }
    let toplevel = run_git(&path, &git_args(&["rev-parse", "--show-toplevel"]))
        .map_err(|_| format!("Not a git repository: {}", path.display()))?;
    let worktree = toplevel.trim();
    if worktree.is_empty() {
        return Err(format!("Not a git repository: {}", path.display()));
    }
    let worktree = PathBuf::from(worktree);
    toplevel_cache()
        .lock()
        .unwrap()
        .insert(path, worktree.clone());
    Ok(worktree)
}
// 3. Argument readers ---------------------------------------------------------
pub(super) fn string_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}
// 4. Test fixtures ------------------------------------------------------------
#[cfg(test)]
pub(super) fn temp_repo(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!(
            "{prefix}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&dir).unwrap();
    run_git(&dir, &git_args(&["init"])).unwrap();
    dir
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn open_repo_caches_and_invalidates_toplevel() {
        let dir = temp_repo("rust-fs-mcp-tlcache");
        let args = json!({ "path": dir.display().to_string() });
        let first = open_repo(&args).unwrap();
        let second = open_repo(&args).unwrap();
        assert_eq!(first, second);
        // .git 마커가 사라지면 캐시를 버리고 다시 해석해야 한다(상위 리포로 폴백).
        let moved = dir.join(".git-moved");
        std::fs::rename(dir.join(".git"), &moved).unwrap();
        let reresolved = open_repo(&args).unwrap();
        assert_ne!(reresolved, first);
        std::fs::rename(&moved, dir.join(".git")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
