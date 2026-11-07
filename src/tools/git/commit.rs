//! commit.rs
//! tools::git::commit
//!
//! git-commit and git-amend handlers with Conventional Commit checks and identity resolution.
//! filesToStage commits through `commit --only` so already staged entries stay staged.
//!

use super::repo::{git_args, open_repo, run_git, string_array};
use crate::core::args_ref::read_text_slice;
use crate::core::config::ensure_path_allowed;
use crate::core::response::RawResult;
use crate::tools::common::bool_field;
use serde_json::{Value, json};
use std::path::Path;

// 1. Commit tools -------------------------------------------------------------
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
        // --amend 는 author date 를 원래 커밋 시각으로 보존하므로 amend 시각으로 갱신한다.
        command.push("--date=now".to_string());
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
    // 커밋 후 oid 프로브: show 1회로 oid/subject를 함께 읽는 헬퍼를 쓴다(스폰 수 통일).
    let (oid, _) = head_oid_subject(&worktree);

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
    if let Some(message) = &new_message
        && !looks_conventional(message)
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
    // --amend 는 author date 를 원래 커밋 시각으로 보존하므로 --date=now 로
    // 커밋 시간을 amend 시각으로 갱신한다(committer date 는 항상 현재).
    command.push("--date=now".to_string());
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
    // --no-edit keeps the old header, so read the subject back from HEAD for the echo line.
    // oid와 subject를 show 1회로 함께 읽어 rev-parse + show 이중 스폰을 합친다.
    let (oid, head_subject) = head_oid_subject(&worktree);
    let subject = match &new_message {
        Some(message) => first_line(message).to_string(),
        None => head_subject,
    };

    RawResult::structured(
        format!("[{oid}] {subject}"),
        json!({
            "path": worktree.display().to_string(),
            "oid": oid
        }),
    )
}
// 2. Message and identity helpers ---------------------------------------------
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
    if args.get("message").and_then(Value::as_str).is_some()
        || args.get("messagePath").and_then(Value::as_str).is_some()
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
// HEAD의 oid와 subject를 git show 1회로 함께 읽는다(커밋/어멘드 후 프로브 스폰 절감).
fn head_oid_subject(worktree: &Path) -> (String, String) {
    let Ok(output) = run_git(
        worktree,
        &git_args(&["show", "-s", "--format=%H%n%s", "HEAD"]),
    ) else {
        return (String::new(), String::new());
    };
    let mut lines = output.lines();
    let oid = lines.next().unwrap_or("").trim().to_string();
    let subject = lines.next().unwrap_or("").trim().to_string();
    (oid, subject)
}
// user.name/user.email 을 git config 1회 호출로 읽는다(system>global>local 순 출력, 마지막 값 우선).
// worktree별 캐시로 커밋당 spawn 1회를 지울 수 있으나 추가하지 않음: .git 이 파일인 링크된
// worktree·서브모듈은 .git/config 가 없고 system·XDG(git/config) 설정도 mtime 무효화가
// 닿지 않아, 낡은 신원이 rust-fs-mcp 폴백으로 커밋되는 회귀 위험이 남음. 커밋은
// 사용자 주도 저변도 경로라 신원 정확성을 우선함.
fn config_identity(worktree: &Path) -> (Option<String>, Option<String>) {
    let Ok(output) = run_git(worktree, &git_args(&["config", "--get-regexp", "^user\\."])) else {
        return (None, None);
    };
    let mut name = None;
    let mut email = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("user.name ") {
            name = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("user.email ") {
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
#[cfg(test)]
mod tests {
    use super::super::repo::temp_repo;
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
        run_git(
            &dir,
            &git_args(&["config", "user.email", "only@example.com"]),
        )
        .unwrap();
        std::fs::write(dir.join("staged.txt"), "staged").unwrap();
        run_git(&dir, &git_args(&["add", "--", "staged.txt"])).unwrap();
        std::fs::write(dir.join("target.txt"), "target").unwrap();
        let commit = handle_git_commit(&json!({
            "path": dir.display().to_string(),
            "message": "test: isolated commit",
            "filesToStage": ["target.txt"]
        }));
        assert!(!commit.is_error, "{commit:?}");
        let files = run_git(
            &dir,
            &git_args(&["show", "--name-only", "--format=", "HEAD"]),
        )
        .unwrap();
        assert_eq!(files.trim(), "target.txt");
        let status = run_git(&dir, &git_args(&["status", "--porcelain"])).unwrap();
        assert!(status.contains("A  staged.txt"), "{status}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn amend_files_to_stage_isolates_existing_staged() {
        let dir = temp_repo("rust-fs-mcp-amend-only");
        run_git(&dir, &git_args(&["config", "user.name", "Only Tester"])).unwrap();
        run_git(
            &dir,
            &git_args(&["config", "user.email", "only@example.com"]),
        )
        .unwrap();
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
        let files = run_git(
            &dir,
            &git_args(&["show", "--name-only", "--format=", "HEAD"]),
        )
        .unwrap();
        assert_eq!(files.trim(), "base.txt");
        let status = run_git(&dir, &git_args(&["status", "--porcelain"])).unwrap();
        assert!(status.contains("A  staged.txt"), "{status}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn amend_refreshes_commit_date() {
        // git commit --amend 는 기본으로 author date 를 원래 커밋 시각으로 보존한다.
        // amend 결과 커밋 시간은 amend 시각이어야 한다(--date=now 주입 검증).
        let dir = temp_repo("rust-fs-mcp-amend-date");
        run_git(&dir, &git_args(&["config", "user.name", "Date Tester"])).unwrap();
        run_git(
            &dir,
            &git_args(&["config", "user.email", "date@example.com"]),
        )
        .unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        run_git(&dir, &git_args(&["add", "--", "a.txt"])).unwrap();
        run_git(
            &dir,
            &git_args(&[
                "commit",
                "--date=2000-01-01T00:00:00+0000",
                "-m",
                "test: old date",
            ]),
        )
        .unwrap();
        let amend = handle_git_amend(&json!({ "path": dir.display().to_string() }));
        assert!(!amend.is_error, "{amend:?}");
        let dated = run_git(
            &dir,
            &git_args(&["show", "-s", "--format=%ad", "--date=iso-strict", "HEAD"]),
        )
        .unwrap();
        assert!(!dated.trim().starts_with("2000-01-01"), "{dated}");

        // git-commit 의 amend 플래그 경로도 같은 시간 갱신 계약을 지켜야 한다.
        run_git(
            &dir,
            &git_args(&[
                "commit",
                "--amend",
                "--no-edit",
                "--date=2000-01-01T00:00:00+0000",
            ]),
        )
        .unwrap();
        let commit = handle_git_commit(&json!({
            "path": dir.display().to_string(),
            "message": "test: amend via commit flag",
            "amend": true
        }));
        assert!(!commit.is_error, "{commit:?}");
        let dated = run_git(
            &dir,
            &git_args(&["show", "-s", "--format=%ad", "--date=iso-strict", "HEAD"]),
        )
        .unwrap();
        assert!(!dated.trim().starts_with("2000-01-01"), "{dated}");
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
    fn head_oid_subject_reads_oid_and_subject_in_one_show() {
        // rev-parse + show 이중 스폰을 show 1회로 합친 경로: oid 와 subject 모두 채워짐.
        let dir = temp_repo("rust-fs-mcp-headprobe");
        let commit = handle_git_commit(&json!({
            "path": dir.display().to_string(),
            "message": "test: head probe subject",
            "allowEmpty": true
        }));
        assert!(!commit.is_error, "{commit:?}");
        let (oid, subject) = head_oid_subject(&dir);
        assert!(
            !oid.is_empty() && oid.chars().all(|ch| ch.is_ascii_hexdigit()),
            "{oid}"
        );
        assert_eq!(subject, "test: head probe subject");
        let structured = commit.structured.unwrap();
        assert_eq!(structured["oid"].as_str().unwrap(), oid);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
