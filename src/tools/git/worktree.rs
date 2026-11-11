//! worktree.rs
//! tools::git::worktree
//!
//! git-status and git-add handlers over the working tree and index.
//!

use super::repo::{git_args, open_repo, run_git, string_array};
use crate::core::response::RawResult;
use crate::tools::args::common::bool_field;
use serde_json::{Value, json};
use std::path::Path;

// 1. Status and index tools ---------------------------------------------------
pub fn handle_git_status(args: &Value) -> RawResult {
    let worktree = match open_repo(args) {
        Ok(worktree) => worktree,
        Err(error) => return RawResult::error(error),
    };

    // The porcelain body ships in text once; the previous structured.status copy plus the
    // structured.entries line array sent the same output three times in one envelope.
    match status_text(&worktree, bool_field(args, "includeUntracked", true)) {
        Ok(status) => {
            RawResult::structured(status, json!({ "path": worktree.display().to_string() }))
        }
        Err(error) => RawResult::error(error),
    }
}
fn status_text(worktree: &Path, include_untracked: bool) -> Result<String, String> {
    let mut parts = vec!["status", "--porcelain", "--branch"];
    if !include_untracked {
        parts.push("--untracked-files=no");
    }
    let output = run_git(worktree, &git_args(&parts))?;
    Ok(output.trim_end().to_string())
}
pub fn handle_git_add(args: &Value) -> RawResult {
    let worktree = match open_repo(args) {
        Ok(worktree) => worktree,
        Err(error) => return RawResult::error(error),
    };
    let mut paths = string_array(args, "paths").unwrap_or_default();
    if paths.is_empty()
        && let Some(single) = args.get("path").and_then(Value::as_str)
    {
        paths.push(single.to_string());
    }
    let all = bool_field(args, "all", false);
    let update = bool_field(args, "update", false);
    if paths.is_empty() && !all && !update {
        return RawResult::error("paths or path is required unless all or update is set");
    }
    let mut command = git_args(["add"].as_slice());
    if all {
        command.push("--all".to_string());
    }
    if update {
        command.push("--update".to_string());
    }
    if bool_field(args, "force", false) {
        command.push("--force".to_string());
    }
    command.push("--".to_string());
    command.extend(paths.iter().cloned());
    if let Err(error) = run_git(&worktree, &command) {
        return RawResult::error(error);
    }
    let summary = if paths.is_empty() {
        "Updated index".to_string()
    } else {
        format!("Updated index with {} paths", paths.len())
    };
    RawResult::structured(
        summary,
        json!({
            "path": worktree.display().to_string(),
            "entries": paths.len()
        }),
    )
}
#[cfg(test)]
mod tests {
    use super::super::repo::temp_repo;
    use super::*;

    #[test]
    fn git_status_prints_non_ascii_paths_unquoted() {
        // 명령행 quotepath 우선
        let dir = temp_repo("rust-fs-mcp-quotepath");
        run_git(&dir, &git_args(&["config", "core.quotepath", "true"])).unwrap();
        std::fs::write(dir.join("한글파일.txt"), "x").unwrap();
        let status = handle_git_status(&json!({ "path": dir.display().to_string() }));
        assert!(!status.is_error, "{status:?}");
        let text = status.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("한글파일.txt"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn git_status_requires_path() {
        let result = handle_git_status(&json!({}));
        assert!(result.is_error, "{result:?}");
        assert_eq!(result.content[0]["text"], "Error: path is required");
    }
}
