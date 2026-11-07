//! inspect.rs
//! tools::git::inspect
//!
//! Read-only history handlers: git-diff (patch / name-only / stat / check) and multi-revision git-show.
//! Option-like revisions are rejected before git sees them.
//!

use super::repo::{GIT_TIMEOUT_MS, git_args, open_repo, run_git, string_array};
use crate::core::external::{ExternalTool, run_external};
use crate::core::response::RawResult;
use crate::tools::common::bool_field;
use serde_json::{Value, json};
use std::path::Path;

// 1. Diff tool ----------------------------------------------------------------
pub fn handle_git_diff(args: &Value) -> RawResult {
    // 선행 '-' 값은 git이 옵션(--output 등)으로 해석해 샌드박스 밖 임의 파일에 쓸 수 있어 거부.
    let source = args.get("source").and_then(Value::as_str);
    let target = args.get("target").and_then(Value::as_str);
    for value in [source, target].into_iter().flatten() {
        if value.starts_with('-') {
            return RawResult::error(format!("revision must not start with '-': {value}"));
        }
    }
    let worktree = match open_repo(args) {
        Ok(worktree) => worktree,
        Err(error) => return RawResult::error(error),
    };

    let check = bool_field(args, "check", false);

    let mut command: Vec<String> = vec!["diff".to_string()];
    if check {
        // --check inspects the diff for whitespace errors and leftover conflict markers
        // instead of emitting a patch, so it supersedes the --name-only / --stat / --unified
        // output shapes; source/target/staged/paths still select which diff is inspected.
        command.push("--check".to_string());
    } else if bool_field(args, "nameOnly", false) {
        command.push("--name-only".to_string());
    } else if bool_field(args, "stat", false) {
        command.push("--stat".to_string());
    } else if let Some(context) = args.get("contextLines").and_then(Value::as_u64) {
        command.push(format!("--unified={context}"));
    }
    if bool_field(args, "staged", false) {
        command.push("--staged".to_string());
    } else {
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
    if check {
        return run_diff_check(&worktree, &command);
    }
    let output = match run_git(&worktree, &command) {
        Ok(output) => output.trim_end().to_string(),
        Err(error) => return RawResult::error(error),
    };

    // The diff body ships in text once instead of doubling as structured.diff.
    RawResult::structured(output, json!({ "path": worktree.display().to_string() }))
}
// `git diff --check` lists whitespace errors and leftover conflict markers, exiting with
// status 2 when any are found. That non-zero exit is a successful check result, not a git
// failure, so it is surfaced as clean=false with the offending lines; a genuine git error
// (bad revision, exit 128) still propagates as an error.
fn run_diff_check(worktree: &Path, command: &[String]) -> RawResult {
    let result = run_external(
        ExternalTool::Git,
        command,
        Some(worktree),
        Some(GIT_TIMEOUT_MS),
    );
    let output = match result {
        Ok(output) => output,
        Err(error) => return RawResult::error(error),
    };
    let path_label = worktree.display().to_string();
    match output.status_code {
        Some(0) => RawResult::structured(
            "No whitespace errors or conflict markers".to_string(),
            json!({ "path": path_label, "clean": true }),
        ),
        Some(code) if code > 0 && code < 128 => RawResult::structured(
            output.stdout.trim_end().to_string(),
            json!({ "path": path_label, "clean": false }),
        ),
        other => {
            let detail = if output.stderr.trim().is_empty() {
                output.stdout.trim().to_string()
            } else {
                output.stderr.trim().to_string()
            };
            RawResult::error(format!("git failed (code {other:?}): {detail}"))
        }
    }
}
// 2. Show tool ----------------------------------------------------------------
pub fn handle_git_show(args: &Value) -> RawResult {
    let mut objects = string_array(args, "objects").unwrap_or_default();
    let from_single = objects.is_empty();
    if from_single && let Some(object) = args.get("object").and_then(Value::as_str) {
        objects.push(object.to_string());
    }
    if objects.is_empty() {
        return RawResult::error("object or objects is required");
    }
    // object 문자열에 섞여 온 " --stat" 플래그는 stat 필드로 승격하고 rev만 남긴다.
    let mut stat_flag = bool_field(args, "stat", false);
    for object in objects.iter_mut() {
        if !object.trim().contains(char::is_whitespace) {
            continue;
        }
        let mut parts = object.split_whitespace();
        let head = parts.next().unwrap_or("").to_string();
        for extra in parts {
            if extra == "--stat" {
                stat_flag = true;
            } else {
                return RawResult::error(format!(
                    "unsupported token '{extra}' in object '{object}'; pass options via schema fields (stat, format, filePath)"
                ));
            }
        }
        *object = head;
    }
    // 선행 '-' object는 git이 옵션(--output 등)으로 해석해 임의 파일 쓰기가 되므로 거부.
    for object in &objects {
        if object.starts_with('-') {
            return RawResult::error(format!("object must not start with '-': {object}"));
        }
    }
    let worktree = match open_repo(args) {
        Ok(worktree) => worktree,
        Err(error) => return RawResult::error(error),
    };

    // Every requested revision goes to one git invocation, so a multi-revision history query
    // costs a single tool round-trip; stat / format=raw control the body instead of always
    // returning the full patch. filePath pairs with each revision as object:filePath.
    let mut command = git_args(&["show"]);
    if stat_flag {
        command.push("--stat".to_string());
    }
    if args.get("format").and_then(Value::as_str) == Some("raw") {
        command.push("--format=raw".to_string());
    }
    let file_path = args.get("filePath").and_then(Value::as_str);
    for object in &objects {
        match file_path {
            // object가 이미 rev:path 형태면 filePath를 중복 결합하지 않는다.
            Some(file) if !object.contains(':') => command.push(format!("{object}:{file}")),
            _ => command.push(object.clone()),
        }
    }
    let output = match run_git(&worktree, &command) {
        Ok(output) => output.trim_end().to_string(),
        Err(error) => return RawResult::error(error),
    };

    // The show body ships in text once instead of doubling as structured.output.
    let path_label = worktree.display().to_string();
    let structured = if from_single {
        json!({ "path": path_label, "object": objects[0] })
    } else {
        json!({ "path": path_label, "objects": objects })
    };
    RawResult::structured(output, structured)
}
#[cfg(test)]
mod tests {
    use super::super::handle_git_commit;
    use super::super::repo::temp_repo;
    use super::*;

    #[test]
    fn git_diff_rejects_option_like_revision() {
        let result = handle_git_diff(&json!({ "source": "--output=escape" }));
        assert!(result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or("");
        assert!(text.contains("must not start with '-'"), "{text}");
    }
    #[test]
    fn git_show_absorbs_stat_token_from_object() {
        let dir = temp_repo("rust-fs-mcp-show-stat");
        let commit = handle_git_commit(&json!({
            "path": dir.display().to_string(),
            "message": "test: create show fixture",
            "allowEmpty": true
        }));
        assert!(!commit.is_error, "{commit:?}");
        let result =
            handle_git_show(&json!({ "path": dir.display().to_string(), "object": "HEAD --stat" }));
        assert!(!result.is_error, "{result:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn git_show_rejects_unknown_token_in_object() {
        let result = handle_git_show(&json!({ "object": "HEAD --patch" }));
        assert!(result.is_error, "{result:?}");
    }
    #[test]
    fn git_show_rejects_option_like_object() {
        let result = handle_git_show(&json!({ "object": "--output=escape" }));
        assert!(result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or("");
        assert!(text.contains("must not start with '-'"), "{text}");
    }
}
