use rust_fs_mcp::{catalog, tools};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// 1. Public tool matrix -------------------------------------------------------
#[test]
fn every_catalog_tool_has_a_live_dispatch_path() {
    let root = temp_dir("rust-fs-mcp-tool-matrix");
    let work = root.join("work");
    let repo = root.join("repo");
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&repo).unwrap();

    let mut tested = Vec::new();

    run_ok(
        "dir-create",
        json!({ "path": work.join("nested").display().to_string() }),
        &mut tested,
    );
    let file = work.join("nested").join("sample.txt");
    run_ok(
        "file-write",
        json!({ "path": file.display().to_string(), "content": "alpha\nbeta\ngamma\n" }),
        &mut tested,
    );
    run_ok(
        "file-read",
        json!({ "path": file.display().to_string() }),
        &mut tested,
    );
    run_ok(
        "file-read-line-range",
        json!({ "path": file.display().to_string(), "start_line": 2, "line_count": 1 }),
        &mut tested,
    );
    run_ok(
        "dir-list",
        json!({ "path": work.display().to_string(), "depth": 2, "includeFiles": true }),
        &mut tested,
    );
    run_ok(
        "path-stat",
        json!({ "path": file.display().to_string() }),
        &mut tested,
    );
    run_ok(
        "fs-search",
        json!({ "path": work.display().to_string(), "pattern": "beta", "filePattern": "*.txt" }),
        &mut tested,
    );
    run_ok(
        "file-edit",
        json!({ "file_path": file.display().to_string(), "old_string": "beta", "new_string": "BETA" }),
        &mut tested,
    );
    run_ok(
        "file-edit-lines",
        json!({ "file_path": file.display().to_string(), "start_line": 3, "replacement": "GAMMA\n" }),
        &mut tested,
    );
    run_ok(
        "file-replace",
        json!({ "file_path": file.display().to_string(), "pattern": "G(AM)MA", "replacement": "g${1}ma", "expectedReplacements": 1 }),
        &mut tested,
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "alpha\nBETA\ngAMma\n");
    run_ok(
        "file-diff",
        json!({ "path": file.display().to_string(), "content": "alpha\nBETA\n" }),
        &mut tested,
    );
    run_ok(
        "path-find",
        json!({ "path": work.display().to_string(), "glob": "*.txt", "type": "file" }),
        &mut tested,
    );
    let code = work.join("main.rs");
    fs::write(&code, "fn main() {\n    let x = a.unwrap();\n}\n").unwrap();
    let ast_args =
        json!({ "path": code.display().to_string(), "pattern": "$A.unwrap()", "lang": "rust" });
    if ast_grep_on_path() {
        run_ok("ast-search", ast_args, &mut tested);
    } else {
        // CLI 부재 경로 (설치 안내 오류)
        let result = tools::dispatch_tool_call("ast-search", Some(ast_args));
        assert_eq!(result["isError"], true);
        assert!(result_text(&result).contains("ast-grep"), "{result}");
        tested.push("ast-search".to_string());
    }
    let copied = work.join("copy.txt");
    run_ok(
        "path-copy",
        json!({ "source": file.display().to_string(), "destination": copied.display().to_string() }),
        &mut tested,
    );
    let moved = work.join("moved.txt");
    run_ok(
        "path-move",
        json!({ "source": copied.display().to_string(), "destination": moved.display().to_string() }),
        &mut tested,
    );
    run_ok(
        "path-remove",
        json!({ "path": moved.display().to_string() }),
        &mut tested,
    );

    init_repo(&repo);
    fs::write(repo.join("a.txt"), "one\n").unwrap();
    run_ok(
        "git-status",
        json!({ "path": repo.display().to_string() }),
        &mut tested,
    );
    run_ok(
        "git-add",
        json!({ "path": repo.display().to_string(), "paths": ["a.txt"] }),
        &mut tested,
    );
    run_ok(
        "git-commit",
        json!({ "path": repo.display().to_string(), "message": "test: initial\n- add a" }),
        &mut tested,
    );
    fs::write(repo.join("a.txt"), "two\n").unwrap();
    run_ok(
        "git-diff",
        json!({ "path": repo.display().to_string(), "paths": ["a.txt"] }),
        &mut tested,
    );
    run_ok(
        "git-show",
        json!({ "path": repo.display().to_string(), "object": "HEAD", "stat": true }),
        &mut tested,
    );
    run_ok(
        "git-add",
        json!({ "path": repo.display().to_string(), "paths": ["a.txt"] }),
        &mut tested,
    );
    run_ok(
        "git-amend",
        json!({ "path": repo.display().to_string(), "message": "test: amended\n- update a" }),
        &mut tested,
    );

    let fetch = serve_once("matrix fetch");
    run_ok(
        "web-fetch",
        json!({ "url": fetch.url, "dump": "text" }),
        &mut tested,
    );
    fetch.join.join().unwrap();
    run_ok(
        "web-extract",
        json!({ "html": "<h1>Title</h1><p>Body</p>", "dump": "text" }),
        &mut tested,
    );
    let download = serve_once("download body");
    run_ok(
        "download-to-file",
        json!({ "items": [{ "url": download.url, "path": work.join("download.txt").display().to_string() }] }),
        &mut tested,
    );
    download.join.join().unwrap();

    let _ = fs::remove_dir_all(&root);
    assert_all_catalog_tools_were_tested(tested);
}

#[test]
fn oversized_batch_read_is_truncated_under_client_limit() {
    let root = temp_dir("rust-fs-mcp-output-budget");
    fs::create_dir_all(&root).unwrap();
    // The combined batch total (~216KB) far exceeds the re-enabled output budget; the server
    // truncates the result so it stays under the client MCP output-token cap instead of forcing
    // the client to dump the overflow to a file.
    let mut paths = Vec::new();
    for index in 0..3 {
        let path = root.join(format!("big-{index}.txt"));
        let body =
            format!("payload line {index} abcdefghijklmnopqrstuvwxyz0123456789\n").repeat(1_200);
        fs::write(&path, &body).unwrap();
        paths.push(path.display().to_string());
    }

    let result = tools::dispatch_tool_call("file-read", Some(json!({ "paths": paths })));
    assert!(result.get("isError").is_none(), "{}", result_text(&result));
    let standard = serde_json::to_string(&result["structuredContent"]).unwrap();
    assert!(
        standard.len() < 30_000,
        "structuredContent must stay under the client output ceiling, got {} bytes",
        standard.len()
    );
    assert!(result_text(&result).contains("[truncated: kept"));
    assert_eq!(
        result["_meta"]["fsMcpResult"]["outputTruncated"],
        json!(true)
    );
    let batch_text = result_text(&result);
    for index in 0..3 {
        assert!(
            batch_text.contains(&format!("payload line {index}")),
            "missing batch item {index}"
        );
        assert_eq!(
            result["structuredContent"]["data"]["structuredContent"]["results"][index]["truncated"],
            true
        );
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn path_remove_deletes_via_simple_paths_array() {
    // path-stat/dir-create 처럼 단순 paths 배열로 삭제할 수 있어야 한다(과거 items 강제 → 스키마 오용 실패).
    let root = temp_dir("rust-fs-mcp-remove-paths");
    fs::create_dir_all(&root).unwrap();
    let first = root.join("one.txt");
    let second = root.join("two.txt");
    fs::write(&first, "1").unwrap();
    fs::write(&second, "2").unwrap();

    let result = tools::dispatch_tool_call(
        "path-remove",
        Some(json!({ "paths": [first.display().to_string(), second.display().to_string()] })),
    );
    assert!(result.get("isError").is_none(), "{}", result_text(&result));
    assert!(!first.exists(), "first path should be deleted");
    assert!(!second.exists(), "second path should be deleted");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mysql_query_without_config_reports_how_to_enable_it() {
    // cargo test는 RUST_FS_MCP_CONFIG=none ([mysql] 미설정)
    let result = tools::dispatch_tool_call("mysql-query", Some(json!({ "sql": "SELECT 1" })));
    assert_eq!(result["isError"], true);
    let reason = result["_meta"]["fsMcpResult"]["errorMessage"]
        .as_str()
        .unwrap_or_default();
    assert!(reason.contains("not configured"), "{result}");
    assert!(reason.contains("[mysql]"), "{result}");
}

#[test]
fn file_replace_ast_mode_rewrites_code_only() {
    if !ast_grep_on_path() {
        return;
    }
    let root = temp_dir("rust-fs-mcp-ast-replace");
    fs::create_dir_all(&root).unwrap();
    let code = root.join("lib.rs");
    let source = "fn f() {\n    let 값 = a.unwrap();\n    // b.unwrap() stays\n    let s = \"c.unwrap()\";\n}\n";
    fs::write(&code, source).unwrap();
    let args = |dry_run: bool| {
        json!({
            "file_path": code.display().to_string(),
            "pattern": "$A.unwrap()",
            "replacement": "$A.expect(\"checked\")",
            "mode": "ast",
            "lang": "rust",
            "expectedReplacements": 1,
            "dryRun": dry_run
        })
    };

    // 미리보기 (무기록)
    let preview = tools::dispatch_tool_call("file-replace", Some(args(true)));
    assert!(
        preview.get("isError").is_none(),
        "{}",
        result_text(&preview)
    );
    assert!(
        result_text(&preview).contains("+    let 값 = a.expect(\"checked\");"),
        "{}",
        result_text(&preview)
    );
    assert_eq!(fs::read_to_string(&code).unwrap(), source);

    // 적용 (주석·문자열 유지)
    let applied = tools::dispatch_tool_call("file-replace", Some(args(false)));
    assert!(
        applied.get("isError").is_none(),
        "{}",
        result_text(&applied)
    );
    assert_eq!(
        fs::read_to_string(&code).unwrap(),
        source.replace("a.unwrap()", "a.expect(\"checked\")")
    );

    let _ = fs::remove_dir_all(&root);
}

// 2. Assertions ---------------------------------------------------------------
fn run_ok(name: &str, args: Value, tested: &mut Vec<String>) -> Value {
    let result = tools::dispatch_tool_call(name, Some(args));
    assert!(
        result.get("isError").is_none(),
        "{name} failed: {}",
        result_text(&result)
    );
    tested.push(name.to_string());
    result
}

fn assert_all_catalog_tools_were_tested(mut tested: Vec<String>) {
    tested.sort();
    tested.dedup();
    let mut catalog = catalog::tool_catalog()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    catalog.sort();
    assert_eq!(tested, catalog);
}

fn result_text(result: &Value) -> String {
    result["structuredContent"]["data"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

// 3. Test fixtures ------------------------------------------------------------
struct TestServer {
    url: String,
    join: thread::JoinHandle<()>,
}

fn serve_once(body: &'static str) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let join = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => request.extend_from_slice(&buffer[..read]),
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        // 응답 후 연결 유지 (127.0.0.1 리셋 회피)
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        while matches!(stream.read(&mut buffer), Ok(read) if read > 0) {}
    });
    TestServer { url, join }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("{prefix}-{stamp}"))
}

fn ast_grep_on_path() -> bool {
    Command::new("ast-grep")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn init_repo(path: &Path) {
    let status = Command::new("git")
        .args(["init"])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(status.success(), "git init failed");
}
