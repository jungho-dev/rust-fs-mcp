//! git_tools.rs
//! tools::git_tools
//!
//! Collection of git tool handlers that invoke the git CLI resolved from PATH.
//! cwd / status / add / commit / diff / show all preserve the structuredContent key contract.
//!

use crate::core::args_ref::read_text_slice;
use crate::core::external::{ExternalTool, run_external};
use crate::core::config::ensure_path_allowed;
use crate::core::response::RawResult;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static GIT_CWD: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

const GIT_TIMEOUT_MS: u64 = 120_000;

fn git_cwd() -> &'static Mutex<Option<PathBuf>> {
    GIT_CWD.get_or_init(|| Mutex::new(None))
}

// Run a git command. On success (exit 0) returns stdout; on failure returns an stderr-based error.
fn run_git(cwd: &Path, args: &[String]) -> Result<String, String> {
    let output = run_external(ExternalTool::Git, args, Some(cwd), Some(GIT_TIMEOUT_MS))?;
    if output.status_code == Some(0) {
        return Ok(output.stdout);
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

fn git_args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| part.to_string()).collect()
}

// Resolve the repo location: args.path or the stored git cwd. The worktree root is pinned via rev-parse.
fn resolve_repo_path(args: &Value) -> Result<PathBuf, String> {
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        let resolved = ensure_path_allowed(path)?;
        // If a file path is supplied use its parent directory as the git invocation cwd.
        if resolved.is_file() {
            return Ok(resolved.parent().map(Path::to_path_buf).unwrap_or(resolved));
        }
        return Ok(resolved);
    }
    git_cwd()
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "path is required (no git cwd set)".to_string())
}

fn open_repo(args: &Value) -> Result<PathBuf, String> {
    let path = resolve_repo_path(args)?;
    let toplevel = run_git(&path, &git_args(&["rev-parse", "--show-toplevel"]))
        .map_err(|_| format!("Not a git repository: {}", path.display()))?;
    let worktree = toplevel.trim();
    if worktree.is_empty() {
        return Err(format!("Not a git repository: {}", path.display()));
    }

    Ok(PathBuf::from(worktree))
}

fn status_text(worktree: &Path, include_untracked: bool) -> Result<String, String> {
    let mut parts = vec!["status", "--porcelain", "--branch"];
    if !include_untracked {
        parts.push("--untracked-files=no");
    }
    let output = run_git(worktree, &git_args(&parts))?;
    Ok(output.trim_end().to_string())
}

// 1. Git tools ----------------------------------------------------------------
pub fn handle_git_cwd(args: &Value) -> RawResult {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };
    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };

    let has_git = path.join(".git").exists();
    if bool_field(args, "initializeIfNotPresent", false) && !has_git {
        if !path.exists()
            && let Err(error) = fs::create_dir_all(&path)
        {
            return RawResult::error(format!("Failed to create {}: {error}", path.display()));
        }
        if let Err(error) = run_git(&path, &git_args(&["init"])) {
            return RawResult::error(error);
        }
    }

    let toplevel = run_git(&path, &git_args(&["rev-parse", "--show-toplevel"]));
    let worktree = match toplevel {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
        _ => {
            if bool_field(args, "validateGitRepo", true) {
                return RawResult::error(format!("Not a git repository: {}", path.display()));
            }
            *git_cwd().lock().unwrap() = Some(path.clone());
            return RawResult::structured(
                format!("Git cwd set to {}", path.display()),
                json!({ "path": path.display().to_string(), "validated": false }),
            );
        }
    };

    *git_cwd().lock().unwrap() = Some(worktree.clone());
    let git_dir = run_git(&worktree, &git_args(&["rev-parse", "--absolute-git-dir"]))
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    let status = status_text(&worktree, true).unwrap_or_default();
    RawResult::structured(
        format!("Git cwd set to {}", worktree.display()),
        json!({
            "path": worktree.display().to_string(),
            "gitDir": git_dir,
            "status": status
        }),
    )
}

pub fn handle_git_status(args: &Value) -> RawResult {
    let worktree = match open_repo(args) {
        Ok(worktree) => worktree,
        Err(error) => return RawResult::error(error),
    };

    match status_text(&worktree, bool_field(args, "includeUntracked", true)) {
        Ok(status) => RawResult::structured(
            status.clone(),
            json!({
                "path": worktree.display().to_string(),
                "status": status,
                "entries": status.lines().collect::<Vec<_>>()
            }),
        ),
        Err(error) => RawResult::error(error),
    }
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
    if paths.is_empty() {
        return RawResult::error("paths or path is required");
    }

    let mut command = git_args(&["add", "--"]);
    command.extend(paths.iter().cloned());
    if let Err(error) = run_git(&worktree, &command) {
        return RawResult::error(error);
    }

    RawResult::structured(
        format!("Updated index with {} paths", paths.len()),
        json!({
            "path": worktree.display().to_string(),
            "entries": paths.len()
        }),
    )
}

pub fn handle_git_commit(args: &Value) -> RawResult {
    let worktree = match open_repo(args) {
        Ok(worktree) => worktree,
        Err(error) => return RawResult::error(error),
    };

    if let Some(files) = string_array(args, "filesToStage")
        && !files.is_empty()
    {
        let mut add = git_args(&["add", "--"]);
        add.extend(files.iter().cloned());
        if let Err(error) = run_git(&worktree, &add) {
            return RawResult::error(error);
        }
    }

    let message = match commit_message(args) {
        Ok(message) => message,
        Err(error) => return RawResult::error(error),
    };
    if !looks_conventional(&message) {
        return RawResult::error(
            "Commit message must start with an English Conventional Commit header",
        );
    }

    let mut command: Vec<String> = Vec::new();
    match author_identity(args) {
        Some(author) => {
            command.push("commit".to_string());
            command.push("--author".to_string());
            command.push(author);
        }
        None => {
            // Inject default user.name / user.email (still works without local git config).
            command.push("-c".to_string());
            command.push("user.name=rust-fs-mcp".to_string());
            command.push("-c".to_string());
            command.push("user.email=rust-fs-mcp@example.invalid".to_string());
            command.push("commit".to_string());
        }
    }
    command.push("-m".to_string());
    command.push(message.clone());
    if bool_field(args, "amend", false) {
        command.push("--amend".to_string());
    }
    if bool_field(args, "allowEmpty", false) {
        command.push("--allow-empty".to_string());
    }

    if let Err(error) = run_git(&worktree, &command) {
        return RawResult::error(error);
    }
    let oid = run_git(&worktree, &git_args(&["rev-parse", "HEAD"]))
        .map(|value| value.trim().to_string())
        .unwrap_or_default();

    RawResult::structured(
        format!("[{oid}] {}", first_line(&message)),
        json!({
            "path": worktree.display().to_string(),
            "oid": oid,
            "message": message
        }),
    )
}

pub fn handle_git_diff(args: &Value) -> RawResult {
    let worktree = match open_repo(args) {
        Ok(worktree) => worktree,
        Err(error) => return RawResult::error(error),
    };

    let mut command: Vec<String> = vec!["diff".to_string()];
    if bool_field(args, "nameOnly", false) {
        command.push("--name-only".to_string());
    } else if bool_field(args, "stat", false) {
        command.push("--stat".to_string());
    }

    if bool_field(args, "staged", false) {
        command.push("--staged".to_string());
    } else {
        let source = args.get("source").and_then(Value::as_str);
        let target = args.get("target").and_then(Value::as_str);
        match (source, target) {
            (Some(source), Some(target)) => {
                command.push(source.to_string());
                command.push(target.to_string());
            }
            (Some(value), None) | (None, Some(value)) => {
                command.push(value.to_string());
            }
            (None, None) => {}
        }
    }

    if let Some(paths) = string_array(args, "paths")
        && !paths.is_empty()
    {
        command.push("--".to_string());
        command.extend(paths);
    }

    let output = match run_git(&worktree, &command) {
        Ok(output) => output.trim_end().to_string(),
        Err(error) => return RawResult::error(error),
    };

    RawResult::structured(
        output.clone(),
        json!({
            "path": worktree.display().to_string(),
            "diff": output
        }),
    )
}

pub fn handle_git_show(args: &Value) -> RawResult {
    let worktree = match open_repo(args) {
        Ok(worktree) => worktree,
        Err(error) => return RawResult::error(error),
    };
    let Some(object) = args.get("object").and_then(Value::as_str) else {
        return RawResult::error("object must be a string");
    };

    let spec = match args.get("filePath").and_then(Value::as_str) {
        Some(file) => format!("{object}:{file}"),
        None => object.to_string(),
    };
    let output = match run_git(&worktree, &git_args(&["show", &spec])) {
        Ok(output) => output.trim_end().to_string(),
        Err(error) => return RawResult::error(error),
    };

    RawResult::structured(
        output.clone(),
        json!({
            "path": worktree.display().to_string(),
            "object": object,
            "output": output
        }),
    )
}

// 2. Argument helpers ---------------------------------------------------------
fn commit_message(args: &Value) -> Result<String, String> {
    if let Some(path) = args.get("messagePath").and_then(Value::as_str) {
        let path = ensure_path_allowed(path)?;
        let offset = args
            .get("messageOffset")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let length = args
            .get("messageLength")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        return read_text_slice(path, offset, length);
    }

    args.get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "message or messagePath is required".to_string())
}

// When an author object is present return "name <email>" form; otherwise fall back to git's default author.
fn author_identity(args: &Value) -> Option<String> {
    let author = args.get("author").and_then(Value::as_object)?;
    let name = author
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("rust-fs-mcp");
    let email = author
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or("rust-fs-mcp@example.invalid");
    Some(format!("{name} <{email}>"))
}

fn looks_conventional(message: &str) -> bool {
    let Some(header) = message.lines().next() else {
        return false;
    };
    let Some((kind, summary)) = header.split_once(": ") else {
        return false;
    };

    let valid_type = kind
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch == '-' || ch == '(' || ch == ')');
    valid_type && summary.chars().any(|ch| ch.is_ascii_alphabetic())
}

fn first_line(value: &str) -> &str {
    value.lines().next().unwrap_or("")
}

fn bool_field(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn string_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conventional_header_detected() {
        assert!(looks_conventional("feat: add thing"));
        assert!(looks_conventional("fix(core): correct path"));
        assert!(!looks_conventional("no type here"));
        assert!(!looks_conventional("WIP"));
    }

    #[test]
    fn author_identity_formats_name_and_email() {
        let args = json!({ "author": { "name": "Jane", "email": "jane@example.com" } });
        assert_eq!(
            author_identity(&args).as_deref(),
            Some("Jane <jane@example.com>")
        );
        assert_eq!(author_identity(&json!({})), None);
    }
}
