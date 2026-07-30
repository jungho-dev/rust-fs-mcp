//! git_tools.rs
//! tools::git_tools
//!
//! Collection of git tool handlers that invoke the git CLI resolved from PATH.
//! cwd / status / add / commit / amend / diff / show all preserve the structuredContent key contract.
//!

use crate::core::args_ref::read_text_slice;
use crate::core::config::ensure_path_allowed;
use crate::core::external::{ExternalTool, run_external};
use crate::core::response::RawResult;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const GIT_TIMEOUT_MS: u64 = 120_000;

// Run a git command. On success (exit 0) returns stdout; on failure returns an stderr-based error.
fn run_git(cwd: &Path, args: &[String]) -> Result<String, String> {
    let output = run_external(ExternalTool::Git, args, Some(cwd), Some(GIT_TIMEOUT_MS))?;
    if output.status_code == Some(0) {
        return Ok(output.stdout);
    }
    let detail = if output.stderr.trim().is_empty() {
        output.stdout.trim().to_string()
    }
    else {
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
fn open_repo(args: &Value) -> Result<PathBuf, String> {
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
fn status_text(worktree: &Path, include_untracked: bool) -> Result<String, String> {
    let mut parts = vec!["status", "--porcelain", "--branch"];
    if !include_untracked {
        parts.push("--untracked-files=no");
    }
    let output = run_git(worktree, &git_args(&parts))?;
    Ok(output.trim_end().to_string())
}
// 1. Git tools ----------------------------------------------------------------
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
pub fn handle_git_add(args: &Value) -> RawResult {
    let worktree = match open_repo(args) {
        Ok(worktree) => worktree,
        Err(error) => return RawResult::error(error),
    };
    let mut paths = string_array(args, "paths").unwrap_or_default();
    if paths.is_empty() && let Some(single) = args.get("path").and_then(Value::as_str)
    {
        paths.push(single.to_string());
    }
    let all = bool_field(args, "all", false);
    let update = bool_field(args, "update", false);
    if paths.is_empty() && !all && !update {
        return RawResult::error("paths or path is required unless all or update is set");
    }
    let mut command = git_args(&["add"]);
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
    }
    else {
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
pub fn handle_git_commit(args: &Value) -> RawResult {
    let worktree = match open_repo(args) {
        Ok(worktree) => worktree,
        Err(error) => return RawResult::error(error),
    };

    // filesToStage는 격리 커밋: add로 index를 바꾸는 대신 commit --only pathspec으로
    // 지정 파일의 워킹트리 내용만 커밋한다. 기존 staged 항목은 staged 상태로 남는다.
    // untracked 파일은 --only가 못 집으므로 intent-to-add로 추적만 시작한다
    // (이미 추적 중인 파일에는 no-op이라 index 내용이 바뀌지 않는다).
    let files = string_array(args, "filesToStage").unwrap_or_default();
    if !files.is_empty() {
        let mut add = git_args(&["add", "--intent-to-add", "--"]);
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
            "Commit message must start with a Conventional Commit header like 'fix: summary'",
        );
    }
    // 신원 우선순위: author 인자 > git config(local/global) > rust-fs-mcp 폴백.
    // -c 무조건 주입은 local config 보다 우선해 author/committer 가 항상
    // rust-fs-mcp 로 기록되던 버그였다. config 부재 시에만 폴백을 주입한다.
    let author = author_identity(args);
    let mut command: Vec<String> = identity_flags(&worktree, author.as_ref());
    command.push("commit".to_string());
    if !files.is_empty() {
        command.push("--only".to_string());
    }
    if let Some((name, email)) = &author {
        command.push("--author".to_string());
        command.push(format!("{name} <{email}>"));
    }
    command.push("-m".to_string());
    command.push(message.clone());
    if bool_field(args, "amend", false) {
        command.push("--amend".to_string());
    }
    if bool_field(args, "allowEmpty", false) {
        command.push("--allow-empty".to_string());
    }
    if bool_field(args, "noVerify", false) {
        command.push("--no-verify".to_string());
    }
    if !files.is_empty() {
        command.push("--".to_string());
        command.extend(files.iter().cloned());
    }
    if let Err(error) = run_git(&worktree, &command) {
        return RawResult::error(error);
    }
    let oid = run_git(&worktree, &git_args(&["rev-parse", "HEAD"]))
        .map(|value| value.trim().to_string())
        .unwrap_or_default();

    // The caller already holds the commit message; echoing it back only doubles tokens.
    RawResult::structured(
        format!("[{oid}] {}", first_line(&message)),
        json!({
            "path": worktree.display().to_string(),
            "oid": oid
        }),
    )
}
// Amend the existing HEAD commit. A new message replaces the header; otherwise --no-edit reuses it.
pub fn handle_git_amend(args: &Value) -> RawResult {
    let worktree = match open_repo(args) {
        Ok(worktree) => worktree,
        Err(error) => return RawResult::error(error),
    };

    // Amend rewrites HEAD, so a commit must already exist; report it clearly instead of git's raw error.
    if run_git(&worktree, &git_args(&["rev-parse", "--verify", "HEAD"])).is_err() {
        return RawResult::error("Cannot amend: repository has no commits yet");
    }
    // --author and --reset-author both rewrite authorship; combining them is ambiguous.
    let author = author_identity(args);
    let reset_author = bool_field(args, "resetAuthor", false);
    if author.is_some() && reset_author {
        return RawResult::error("author and resetAuthor cannot be combined");
    }
    // filesToStage는 격리 amend: commit --only pathspec으로 지정 파일만 반영하고
    // 기존 staged 항목은 staged 상태로 남긴다. untracked는 intent-to-add로 추적 시작.
    let files = string_array(args, "filesToStage").unwrap_or_default();
    if !files.is_empty() {
        let mut add = git_args(&["add", "--intent-to-add", "--"]);
        add.extend(files.iter().cloned());
        if let Err(error) = run_git(&worktree, &add) {
            return RawResult::error(error);
        }
    }
    let new_message = match optional_commit_message(args) {
        Ok(message) => message,
        Err(error) => return RawResult::error(error),
    };
    if let Some(message) = &new_message && !looks_conventional(message)
    {
        return RawResult::error(
            "Commit message must start with a Conventional Commit header like 'fix: summary'",
        );
    }
    // 신원 우선순위: author 인자 > git config > 폴백. --reset-author 는 이 신원으로
    // author 를 다시 쓰므로 config 신원이 그대로 반영된다.
    let mut command: Vec<String> = identity_flags(&worktree, author.as_ref());
    command.push("commit".to_string());
    command.push("--amend".to_string());
    if !files.is_empty() {
        command.push("--only".to_string());
    }
    if let Some((name, email)) = &author {
        command.push("--author".to_string());
        command.push(format!("{name} <{email}>"));
    }
    if reset_author {
        command.push("--reset-author".to_string());
    }
    match &new_message {
        Some(message) => {
            command.push("-m".to_string());
            command.push(message.clone());
        }
        None => command.push("--no-edit".to_string()),
    }
    if bool_field(args, "allowEmpty", false) {
        command.push("--allow-empty".to_string());
    }
    if bool_field(args, "noVerify", false) {
        command.push("--no-verify".to_string());
    }
    if !files.is_empty() {
        command.push("--".to_string());
        command.extend(files.iter().cloned());
    }
    if let Err(error) = run_git(&worktree, &command) {
        return RawResult::error(error);
    }
    let oid = run_git(&worktree, &git_args(&["rev-parse", "HEAD"]))
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    // --no-edit keeps the old header, so read the subject back from HEAD for the echo line.
    let subject = match &new_message {
        Some(message) => first_line(message).to_string(),
        None => run_git(&worktree, &git_args(&["show", "-s", "--format=%s", "HEAD"]))
            .map(|value| value.trim().to_string())
            .unwrap_or_default(),
    };

    RawResult::structured(
        format!("[{oid}] {subject}"),
        json!({
            "path": worktree.display().to_string(),
            "oid": oid
        }),
    )
}
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
    }
    else if bool_field(args, "nameOnly", false) {
        command.push("--name-only".to_string());
    }
    else if bool_field(args, "stat", false) {
        command.push("--stat".to_string());
    }
    else if let Some(context) = args.get("contextLines").and_then(Value::as_u64) {
        command.push(format!("--unified={context}"));
    }
    if bool_field(args, "staged", false) {
        command.push("--staged".to_string());
    }
    else {
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
    if let Some(paths) = string_array(args, "paths") && !paths.is_empty()
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
            }
            else {
                output.stderr.trim().to_string()
            };
            RawResult::error(format!("git failed (code {other:?}): {detail}"))
        }
    }
}
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
            }
            else {
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
    }
    else {
        json!({ "path": path_label, "objects": objects })
    };
    RawResult::structured(output, structured)
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
// Like commit_message but yields None when neither message nor messagePath is given,
// so amend can fall back to --no-edit and keep HEAD's existing message.
fn optional_commit_message(args: &Value) -> Result<Option<String>, String> {
    if args.get("message").and_then(Value::as_str).is_some() || args.get("messagePath").and_then(Value::as_str).is_some()
    {
        return commit_message(args).map(Some);
    }
    Ok(None)
}
// author 객체가 있으면 (name, email) 쌍을, 없으면 None(git config 신원 사용)을 반환.
fn author_identity(args: &Value) -> Option<(String, String)> {
    let author = args.get("author").and_then(Value::as_object)?;
    let name = author
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("rust-fs-mcp");
    let email = author
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or("rust-fs-mcp@example.invalid");
    Some((name.to_string(), email.to_string()))
}
// 커밋터 신원 -c 플래그: author 인자가 있으면 그 신원을 커밋터에도 쓰고,
// 없으면 git config 를 그대로 듐(주입 없음) 부재 항목만 rust-fs-mcp 로 폴백한다.
fn identity_flags(worktree: &Path, author: Option<&(String, String)>) -> Vec<String> {
    if let Some((name, email)) = author {
        return vec![
            "-c".to_string(),
            format!("user.name={name}"),
            "-c".to_string(),
            format!("user.email={email}"),
        ];
    }
    let (name, email) = config_identity(worktree);
    let mut flags = Vec::new();
    if name.is_none() {
        flags.push("-c".to_string());
        flags.push("user.name=rust-fs-mcp".to_string());
    }
    if email.is_none() {
        flags.push("-c".to_string());
        flags.push("user.email=rust-fs-mcp@example.invalid".to_string());
    }
    flags
}
// user.name/user.email 을 git config 1회 호출로 읽는다(system>global>local 순 출력, 마지막 값 우선).
fn config_identity(worktree: &Path) -> (Option<String>, Option<String>) {
    let Ok(output) = run_git(worktree, &git_args(&["config", "--get-regexp", "^user\\."])) else {
        return (None, None);
    };
    let mut name = None;
    let mut email = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("user.name ") {
            name = Some(value.trim().to_string());
        }
        else if let Some(value) = line.strip_prefix("user.email ") {
            email = Some(value.trim().to_string());
        }
    }
    (name, email)
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
    // 요약부는 한글 등 비 ASCII 제목도 허용(사용자 커밋 규약이 한글 제목).
    valid_type && summary.chars().any(char::is_alphabetic)
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
        assert!(looks_conventional("fix: 섹타 단말기 매입취소 기본값 차단"));
        assert!(!looks_conventional("no type here"));
        assert!(!looks_conventional("WIP"));
    }
    #[test]
    fn author_identity_formats_name_and_email() {
        let args = json!({ "author": { "name": "Jane", "email": "jane@example.com" } });
        assert_eq!(
            author_identity(&args),
            Some(("Jane".to_string(), "jane@example.com".to_string()))
        );
        assert_eq!(author_identity(&json!({})), None);
    }
    fn temp_repo(prefix: &str) -> std::path::PathBuf {
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
    #[test]
    fn commit_identity_prefers_config_then_author_arg() {
        // author 인자 없이 커밋하면 local git config 신원이 author/committer 모두에
        // 기록되어야 한다(rust-fs-mcp 강제 주입 회귀 방지).
        let dir = temp_repo("rust-fs-mcp-ident");
        run_git(&dir, &git_args(&["config", "user.name", "Matrix Tester"])).unwrap();
        run_git(
            &dir,
            &git_args(&["config", "user.email", "matrix@example.com"]),
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let commit = handle_git_commit(&json!({
            "path": dir.display().to_string(),
            "message": "test: config identity",
            "filesToStage": [dir.join("a.txt").display().to_string()]
        }));
        assert!(!commit.is_error, "{commit:?}");
        let line = run_git(
            &dir,
            &git_args(&["show", "-s", "--format=%an|%cn|%ae|%ce", "HEAD"]),
        )
        .unwrap();
        assert_eq!(
            line.trim(),
            "Matrix Tester|Matrix Tester|matrix@example.com|matrix@example.com"
        );

        // author 인자를 주면 author 와 committer 가 모두 그 신원이 된다.
        std::fs::write(dir.join("a.txt"), "y").unwrap();
        let authored = handle_git_commit(&json!({
            "path": dir.display().to_string(),
            "message": "test: author arg identity",
            "filesToStage": [dir.join("a.txt").display().to_string()],
            "author": { "name": "Jane", "email": "jane@example.com" }
        }));
        assert!(!authored.is_error, "{authored:?}");
        let line = run_git(&dir, &git_args(&["show", "-s", "--format=%an|%cn", "HEAD"])).unwrap();
        assert_eq!(line.trim(), "Jane|Jane");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn commit_files_to_stage_isolates_existing_staged() {
        // filesToStage 커밋이 기존 staged 항목을 함께 커밋하면 안 된다
        // (실사용 피드백: staged 오염 때문에 shell git으로 폴백하던 건).
        let dir = temp_repo("rust-fs-mcp-only");
        run_git(&dir, &git_args(&["config", "user.name", "Only Tester"])).unwrap();
        run_git(&dir, &git_args(&["config", "user.email", "only@example.com"])).unwrap();
        std::fs::write(dir.join("staged.txt"), "staged").unwrap();
        run_git(&dir, &git_args(&["add", "--", "staged.txt"])).unwrap();
        std::fs::write(dir.join("target.txt"), "target").unwrap();
        let commit = handle_git_commit(&json!({
            "path": dir.display().to_string(),
            "message": "test: isolated commit",
            "filesToStage": ["target.txt"]
        }));
        assert!(!commit.is_error, "{commit:?}");
        let files = run_git(&dir, &git_args(&["show", "--name-only", "--format=", "HEAD"])).unwrap();
        assert_eq!(files.trim(), "target.txt");
        let status = run_git(&dir, &git_args(&["status", "--porcelain"])).unwrap();
        assert!(status.contains("A  staged.txt"), "{status}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn amend_files_to_stage_isolates_existing_staged() {
        let dir = temp_repo("rust-fs-mcp-amend-only");
        run_git(&dir, &git_args(&["config", "user.name", "Only Tester"])).unwrap();
        run_git(&dir, &git_args(&["config", "user.email", "only@example.com"])).unwrap();
        std::fs::write(dir.join("base.txt"), "base").unwrap();
        let base = handle_git_commit(&json!({
            "path": dir.display().to_string(),
            "message": "test: base commit",
            "filesToStage": ["base.txt"]
        }));
        assert!(!base.is_error, "{base:?}");
        std::fs::write(dir.join("staged.txt"), "staged").unwrap();
        run_git(&dir, &git_args(&["add", "--", "staged.txt"])).unwrap();
        std::fs::write(dir.join("base.txt"), "amended").unwrap();
        let amend = handle_git_amend(&json!({
            "path": dir.display().to_string(),
            "filesToStage": ["base.txt"]
        }));
        assert!(!amend.is_error, "{amend:?}");
        let files = run_git(&dir, &git_args(&["show", "--name-only", "--format=", "HEAD"])).unwrap();
        assert_eq!(files.trim(), "base.txt");
        let status = run_git(&dir, &git_args(&["status", "--porcelain"])).unwrap();
        assert!(status.contains("A  staged.txt"), "{status}");
        let _ = std::fs::remove_dir_all(&dir);
    }
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
    #[test]
    fn optional_commit_message_absent_yields_none() {
        assert_eq!(optional_commit_message(&json!({})).unwrap(), None);
        assert_eq!(
            optional_commit_message(&json!({ "message": "feat: add x" })).unwrap(),
            Some("feat: add x".to_string())
        );
    }
    #[test]
    fn git_diff_rejects_option_like_revision() {
        let result = handle_git_diff(&json!({ "source": "--output=escape" }));
        assert!(result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or("");
        assert!(text.contains("must not start with '-'"), "{text}");
    }
    #[test]
    fn git_status_requires_path() {
        let result = handle_git_status(&json!({}));
        assert!(result.is_error, "{result:?}");
        assert_eq!(result.content[0]["text"], "Error: path is required");
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
        let result = handle_git_show(&json!({ "path": dir.display().to_string(), "object": "HEAD --stat" }));
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
