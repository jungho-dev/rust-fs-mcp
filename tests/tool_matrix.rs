use rust_fs_mcp::{catalog, tools};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

// 1. Public tool matrix ----------------------------------------------------------------------
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
    run_ok(
        "fs-inspect",
        json!({
            "root": work.display().to_string(),
            "requests": [
                { "op": "count-files", "path": ".", "glob": "*.txt", "recursive": true },
                { "op": "search", "path": ".", "pattern": "BETA", "filePattern": "*.txt" }
            ]
        }),
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
    run_err(
        "web-render",
        json!({ "url": "http://10.0.0.1/" }),
        &mut tested,
    );

    let _ = fs::remove_dir_all(&root);
    assert_all_catalog_tools_were_tested(tested);
}

// 2. Assertions ------------------------------------------------------------------------------
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

fn run_err(name: &str, args: Value, tested: &mut Vec<String>) -> Value {
    let result = tools::dispatch_tool_call(name, Some(args));
    assert_eq!(
        result["isError"],
        json!(true),
        "{name} unexpectedly succeeded"
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

// 3. Test fixtures --------------------------------------------------------------------------
struct TestServer {
    url: String,
    join: thread::JoinHandle<()>,
}

fn serve_once(body: &'static str) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let join = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 1024];
        let _ = stream.read(&mut buffer);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
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

fn init_repo(path: &Path) {
    let status = Command::new("git")
        .args(["init"])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(status.success(), "git init failed");
}
