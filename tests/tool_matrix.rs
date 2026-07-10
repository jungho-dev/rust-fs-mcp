//! tool_matrix.rs
//! tests::tool_matrix
//!
//! Calls every tool exposed by the tools/list catalog through dispatch_tool_call to verify consistency.
//! Confirms catalog and dispatcher line up one-to-one under both the full and fast-coding profiles.
//!

use rust_fs_mcp::{catalog, server, tools::dispatch_tool_call};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// 1. Full public tool matrix --------------------------------------------------------------
#[test]
fn verifies_full_tool_matrix() {
    let root = temp_root("full-tool-matrix");
    let mut checked = Vec::new();

    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let catalog = catalog_names();
    run_file_tools(&mut checked, &root);
    run_search_tools(&mut checked, &root);
    run_inspect_tools(&mut checked, &root);
    run_git_tools(&mut checked, &root);
    run_web_tools(&mut checked, &root);

    checked.sort();
    checked.dedup();
    assert_eq!(checked, catalog);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn verifies_fast_coding_profile() {
    let names = catalog::tool_catalog_for_profile("fast-coding")
        .into_iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["fs-inspect"]);
}

// 2. File and directory tool coverage --------------------------------------------------------
fn run_file_tools(checked: &mut Vec<String>, root: &Path) {
    let files_dir = root.join("files");
    let nested_dir = files_dir.join("nested");
    let sample = nested_dir.join("sample.txt");
    let copy_path = files_dir.join("copy.txt");
    let moved_path = files_dir.join("moved.txt");
    let gone_path = files_dir.join("gone.txt");

    call_checked(
        checked,
        "dir-create",
        json!({ "paths": [path_text(&nested_dir)] }),
    );
    call_checked(
        checked,
        "file-write",
        json!({
            "items": [{
                "path": path_text(&sample),
                "content": "alpha\nbeta\n"
            }]
        }),
    );
    call_checked(
        checked,
        "file-read",
        json!({ "paths": [path_text(&sample)] }),
    );
    let lines = call_checked(
        checked,
        "file-read-line-range",
        json!({
            "items": [{
                "path": path_text(&sample),
                "start_line": 2,
                "line_count": 1
            }]
        }),
    );
    assert_eq!(first_batch_struct(&lines)["backend"], "native-rust");
    assert_eq!(first_batch_struct(&lines)["returned"], 1);
    assert!(batch_text(&lines).contains("2: beta"));
    let dir = call_checked(
        checked,
        "dir-list",
        json!({
            "items": [{
                "path": path_text(&files_dir),
                "depth": 2,
                "includeFiles": true
            }]
        }),
    );
    assert_eq!(first_batch_struct(&dir)["backend"], "native-rust");
    let listing = batch_text(&dir);
    assert!(listing.contains("nested/sample.txt"));
    let shallow = call_checked(
        checked,
        "dir-list",
        json!({
            "items": [{
                "path": path_text(&files_dir),
                "depth": 1,
                "includeFiles": true
            }]
        }),
    );
    let listing = batch_text(&shallow);
    assert!(listing.contains("nested/"));
    assert!(!listing.contains("nested/sample.txt"));
    call_checked(
        checked,
        "path-copy",
        json!({
            "items": [{
                "source": path_text(&sample),
                "destination": path_text(&copy_path)
            }]
        }),
    );
    call_checked(
        checked,
        "path-move",
        json!({
            "items": [{
                "source": path_text(&copy_path),
                "destination": path_text(&moved_path)
            }]
        }),
    );
    call_checked(
        checked,
        "file-edit",
        json!({
            "items": [{
                "file_path": path_text(&moved_path),
                "old_string": "alpha",
                "new_string": "gamma",
                "expected_replacements": 1
            }]
        }),
    );
    call_checked(
        checked,
        "file-edit-lines",
        json!({
            "items": [{
                "file_path": path_text(&moved_path),
                "start_line": 1,
                "end_line": 1,
                "replacement": "gamma"
            }]
        }),
    );
    call_checked(
        checked,
        "path-stat",
        json!({ "paths": [path_text(&moved_path)] }),
    );
    call_checked(
        checked,
        "file-write",
        json!({
            "items": [{
                "path": path_text(&gone_path),
                "content": "remove me"
            }]
        }),
    );
    call_checked(
        checked,
        "path-remove",
        json!({
            "items": [{
                "path": path_text(&gone_path),
                "force": true
            }]
        }),
    );

    // Flat 단일 인자·별칭 키 호출은 items[]/paths[]로 자동 흡수된다(히스토리 최다 오류 클러스터).
    let flat_path = files_dir.join("flat.txt");
    call_checked(
        checked,
        "file-write",
        json!({ "path": path_text(&flat_path), "content": "flat body" }),
    );
    let flat_read = call_checked(
        checked,
        "file-read",
        json!({ "path": path_text(&flat_path) }),
    );
    assert!(batch_text(&flat_read).contains("flat body"));
    call_checked(
        checked,
        "path-stat",
        json!({ "items": [{ "path": path_text(&flat_path) }] }),
    );
    let flat_list = call_checked(
        checked,
        "dir-list",
        json!({ "path": path_text(&files_dir), "depth": 1 }),
    );
    assert!(batch_text(&flat_list).contains("flat.txt"));
    call_checked(
        checked,
        "file-edit",
        json!({
            "path": path_text(&flat_path),
            "old_string": "flat body",
            "new_string": "flat edited"
        }),
    );
    call_checked(
        checked,
        "file-edit-lines",
        json!({
            "file_path": path_text(&flat_path),
            "start_line": 1,
            "end_line": 1,
            "expected_lines": 1,
            "replacement": "flat final"
        }),
    );
    let alias_copy = files_dir.join("alias-copy.txt");
    call_checked(
        checked,
        "path-copy",
        json!({ "items": [{ "from": path_text(&flat_path), "to": path_text(&alias_copy) }] }),
    );
}

// 3. Search tool coverage -------------------------------------------------------------------
fn run_search_tools(checked: &mut Vec<String>, root: &Path) {
    let search_root = root.join("files");
    let search_args = json!({
        "items": [{
            "path": path_text(&search_root),
            "pattern": "gamma",
            "filePattern": "*.txt",
            "maxResults": 10
        }]
    });

    let regex = call_checked(checked, "fs-search", search_args);
    assert_eq!(first_batch_struct(&regex)["backend"], "path-rg");

    // 기본 제외: node_modules 하위는 검색에서 잘리고, noDefaultExcludes:true 또는
    // 루트가 그 내부일 때는 검색된다.
    let exc_root = std::env::temp_dir().join(format!(
        "rust-fs-mcp-exclude-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let dep_dir = exc_root.join("node_modules").join("pkg");
    fs::create_dir_all(&dep_dir).unwrap();
    fs::write(exc_root.join("app.js"), "needle in app\n").unwrap();
    fs::write(dep_dir.join("dep.js"), "needle in dep\n").unwrap();

    let pruned = call_checked(
        checked,
        "fs-search",
        json!({ "items": [{ "path": path_text(&exc_root), "pattern": "needle" }] }),
    );
    let pruned_text = batch_text(&pruned);
    assert!(pruned_text.contains("app.js"));
    assert!(!pruned_text.contains("dep.js"));

    let full = call_checked(
        checked,
        "fs-search",
        json!({ "items": [{ "path": path_text(&exc_root), "pattern": "needle", "noDefaultExcludes": true }] }),
    );
    assert!(batch_text(&full).contains("dep.js"));

    let inside = call_checked(
        checked,
        "fs-search",
        json!({ "items": [{ "path": path_text(&dep_dir), "pattern": "needle" }] }),
    );
    assert!(batch_text(&inside).contains("dep.js"));
    let _ = fs::remove_dir_all(&exc_root);
}

// 4. Inspect tool coverage ----------------------------------------------------
fn run_inspect_tools(checked: &mut Vec<String>, root: &Path) {
    let inspect_root = root.join("inspect");
    let docs = inspect_root.join("docs");
    let records = inspect_root.join("records");
    let config = inspect_root.join("config");
    let src = inspect_root.join("src");

    fs::create_dir_all(&docs).unwrap();
    fs::create_dir_all(&records).unwrap();
    fs::create_dir_all(&config).unwrap();
    fs::create_dir_all(&src).unwrap();
    fs::write(docs.join("a.md"), "# A\n").unwrap();
    fs::write(docs.join("b.md"), "# B\n").unwrap();
    fs::write(docs.join("c.txt"), "skip\n").unwrap();
    fs::write(records.join("batch-09.jsonl"), "{\"auditCode\":\"MCP-BENCH-FILLER\",\"owner\":\"Wrong\"}\n{\"auditCode\":\"MCP-BENCH-TARGET-042\",\"owner\":\"Han River Logistics\"}\n").unwrap();
    fs::write(
        config.join("service.json"),
        "{\"renewalThreshold\":0.82,\"name\":\"renewal\"}\n",
    )
    .unwrap();
    fs::write(src.join("order-policy.ts"), "const tax = input.subtotal * input.taxRate;\nreturn input.subtotal + tax - input.loyaltyDiscount;\n").unwrap();

    let response = call_checked(
        checked,
        "fs-inspect",
        json!({
            "root": path_text(&inspect_root),
            "requests": [
                {
                    "id": "docs_count",
                    "op": "count-files",
                    "path": "docs",
                    "pattern": "*.md",
                    "recursive": false
                },
                {
                    "id": "target_record",
                    "op": "search",
                    "path": "records",
                    "pattern": "MCP-BENCH-TARGET-042",
                    "literal": true,
                    "filePattern": "*.jsonl",
                    "extract": [{
                        "name": "owner",
                        "regex": "\"owner\"\\s*:\\s*\"([^\"]+)\""
                    }]
                },
                {
                    "id": "service_config",
                    "op": "json-pick",
                    "path": "config/service.json",
                    "pointers": ["/renewalThreshold"]
                },
                {
                    "id": "policy_logic",
                    "op": "snippet",
                    "path": "src/order-policy.ts",
                    "patterns": ["tax", "loyaltyDiscount"],
                    "contextLines": 1
                }
            ]
        }),
    );
    let data = tool_struct(&response);
    assert_eq!(data["status"], "ok");
    assert_eq!(data["answers"][0]["value"]["count"], 2);
    assert_eq!(data["answers"][1]["value"]["owner"], "Han River Logistics");
    assert_eq!(
        data["answers"][2]["value"]["values"]["/renewalThreshold"],
        0.82
    );
    assert_eq!(data["answers"][3]["confidence"], "high");
}

// 5. Git tool coverage ----------------------------------------------------------------------
fn run_git_tools(checked: &mut Vec<String>, root: &Path) {
    let repo = root.join("repo");
    let note = repo.join("note.txt");

    call_checked(
        checked,
        "git-set-workdir",
        json!({
            "path": path_text(&repo),
            "initializeIfNotPresent": true
        }),
    );
    fs::write(&note, "hello\n").unwrap();
    call_checked(checked, "git-add", json!({ "path": path_text(&note) }));
    call_checked(
        checked,
        "git-diff",
        json!({
            "path": path_text(&repo),
            "nameOnly": true
        }),
    );
    let commit = call_checked(
        checked,
        "git-commit",
        json!({
            "path": path_text(&repo),
            "message": "feat: add matrix fixture\n\n- cover git tool surface",
            "author": {
                "name": "rust-fs-mcp",
                "email": "rust-fs-mcp@example.invalid"
            }
        }),
    );
    assert!(tool_struct(&commit)["oid"].as_str().is_some());
    call_checked(
        checked,
        "git-show",
        json!({
            "path": path_text(&repo),
            "object": "HEAD",
            "filePath": "note.txt"
        }),
    );
    fs::write(&note, "hello\nworld\n").unwrap();
    call_checked(checked, "git-add", json!({ "path": path_text(&note) }));
    call_checked(
        checked,
        "git-commit",
        json!({
            "path": path_text(&repo),
            "message": "fix: extend matrix fixture\n\n- add a second revision for git-show coverage"
        }),
    );
    let show_stat = call_checked(
        checked,
        "git-show",
        json!({
            "path": path_text(&repo),
            "object": "HEAD",
            "stat": true
        }),
    );
    let stat_text = batch_text(&show_stat);
    assert!(stat_text.contains("note.txt"));
    assert!(!stat_text.contains("diff --git"));
    let show_multi = call_checked(
        checked,
        "git-show",
        json!({
            "path": path_text(&repo),
            "objects": ["HEAD", "HEAD~1"],
            "stat": true
        }),
    );
    let shown = tool_struct(&show_multi)["objects"].as_array().unwrap();
    assert_eq!(shown.len(), 2);
    assert!(batch_text(&show_multi).matches("commit ").count() >= 2);

    // object에 섞인 " --stat"은 stat으로 승격되고, rev:path object는 filePath와 중복되지 않는다.
    let stat_token = call_checked(
        checked,
        "git-show",
        json!({ "path": path_text(&repo), "objects": ["HEAD --stat"] }),
    );
    assert!(batch_text(&stat_token).contains("note.txt"));
    let colon_object = call_checked(
        checked,
        "git-show",
        json!({
            "path": path_text(&repo),
            "object": "HEAD:note.txt",
            "filePath": "note.txt"
        }),
    );
    assert!(batch_text(&colon_object).contains("hello"));

    // Amend the last commit: stage a change and reuse the message via --no-edit.
    fs::write(&note, "hello\nworld\namend\n").unwrap();
    let amended = call_checked(
        checked,
        "git-amend",
        json!({
            "path": path_text(&repo),
            "filesToStage": [path_text(&note)]
        }),
    );
    let amended_oid = tool_struct(&amended)["oid"].as_str().unwrap().to_string();
    assert!(!amended_oid.is_empty());
    // Amend again with a new conventional message and reset authorship.
    let rewritten = call_checked(
        checked,
        "git-amend",
        json!({
            "path": path_text(&repo),
            "message": "fix: amend matrix fixture\n\n- rewrite head commit message",
            "resetAuthor": true
        }),
    );
    assert_ne!(
        tool_struct(&rewritten)["oid"].as_str().unwrap(),
        amended_oid
    );
    assert!(batch_text(&rewritten).contains("amend matrix fixture"));
    call_checked(
        checked,
        "git-status",
        json!({
            "path": path_text(&repo),
            "includeUntracked": true
        }),
    );

    // git-diff maps contextLines to --unified=<n>; a wide context must surround the change with
    // more lines than a zero context, proving the advertised field reaches the handler.
    let ctx = repo.join("ctx.txt");
    let seed = (1..=20).map(|n| format!("row {n}\n")).collect::<String>();
    fs::write(&ctx, &seed).unwrap();
    call_checked(checked, "git-add", json!({ "path": path_text(&ctx) }));
    call_checked(
        checked,
        "git-commit",
        json!({
            "path": path_text(&repo),
            "message": "test: add context fixture\n\n- exercise git-diff contextLines"
        }),
    );
    fs::write(&ctx, seed.replace("row 10\n", "row 10 changed\n")).unwrap();
    let zero_ctx = call_checked(
        checked,
        "git-diff",
        json!({ "path": path_text(&repo), "contextLines": 0 }),
    );
    let wide_ctx = call_checked(
        checked,
        "git-diff",
        json!({ "path": path_text(&repo), "contextLines": 5 }),
    );
    let count_ctx = |text: &str| text.lines().filter(|line| line.starts_with(' ')).count();
    assert_eq!(count_ctx(batch_text(&zero_ctx)), 0);
    assert!(count_ctx(batch_text(&wide_ctx)) >= 10);

    // git-add honors update/all/force (previously advertised but ignored): stage a tracked
    // modification with update:true and no explicit pathspec, then commit with noVerify:true.
    fs::write(&ctx, seed.replace("row 10\n", "row 10 staged\n")).unwrap();
    let staged = call_checked(
        checked,
        "git-add",
        json!({ "path": path_text(&repo), "update": true }),
    );
    assert!(tool_struct(&staged)["entries"].as_u64().is_some());
    call_checked(
        checked,
        "git-commit",
        json!({
            "path": path_text(&repo),
            "message": "chore: stage via update flag\n\n- exercise git-add update and commit noVerify",
            "noVerify": true
        }),
    );

    // git-diff check runs `git diff --check`: a trailing-whitespace change reports clean:false
    // with the offending line, and exit code 2 (problems found) surfaces as a result, not an error.
    fs::write(&ctx, seed.replace("row 10\n", "row 10 trailing   \n")).unwrap();
    let dirty = call_checked(
        checked,
        "git-diff",
        json!({ "path": path_text(&repo), "check": true }),
    );
    assert_eq!(tool_struct(&dirty)["clean"], json!(false));
    assert!(batch_text(&dirty).contains("trailing whitespace"));
}

// 5b. Web tool coverage ---------------------------------------------------------------------
fn run_web_tools(checked: &mut Vec<String>, root: &Path) {
    // web-extract runs fully offline on inline HTML.
    let extracted = call_checked(
        checked,
        "web-extract",
        json!({ "items": [{ "html": "<h1>Doc</h1><p>Body</p>", "dump": "markdown" }] }),
    );
    assert!(batch_text(&extracted).contains("# Doc"));

    // The network/browser tools are dispatched against a loopback URL so the SSRF guard rejects
    // them deterministically offline: this proves each dispatch arm exists without egress.
    let download_path = root.join("download.bin");
    call_dispatched(checked, "web-fetch", json!({ "url": "http://127.0.0.1/" }));
    call_dispatched(checked, "web-render", json!({ "url": "http://127.0.0.1/" }));
    call_dispatched(
        checked,
        "download-to-file",
        json!({ "items": [{ "url": "http://127.0.0.1/", "path": path_text(&download_path) }] }),
    );
}

// 6. Tool call helpers ---------------------------------------------------------------------
fn call_checked(checked: &mut Vec<String>, name: &str, args: Value) -> Value {
    checked.push(name.to_string());
    let response = dispatch_tool_call(name, Some(args));
    assert_ne!(
        response.get("isError").and_then(Value::as_bool),
        Some(true),
        "{name} failed: {response:#}"
    );
    // The compact envelope keeps {data, durationMs} and adds error only on failure.
    assert!(response["structuredContent"]["data"].is_object());
    assert!(response["structuredContent"].get("error").is_none());
    response
}

// Dispatch a tool that legitimately errors offline (SSRF-blocked / no browser): assert the
// envelope is well formed and it reached a real handler, not the unknown-tool fallback.
fn call_dispatched(checked: &mut Vec<String>, name: &str, args: Value) -> Value {
    checked.push(name.to_string());
    let response = dispatch_tool_call(name, Some(args));
    assert!(
        response["structuredContent"]["data"].is_object(),
        "{name} malformed: {response:#}"
    );
    let message = response["structuredContent"]["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !message.contains("Unknown tool"),
        "{name} not dispatched: {response:#}"
    );
    response
}

fn tool_struct(response: &Value) -> &Value {
    &response["structuredContent"]["data"]["structuredContent"]
}

fn first_batch_struct(response: &Value) -> &Value {
    &tool_struct(response)["results"][0]["data"]
}

fn batch_text(response: &Value) -> &str {
    response["structuredContent"]["data"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
}

fn catalog_names() -> Vec<String> {
    let response =
        server::handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
    let mut names = response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn temp_root(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("rust-fs-mcp-{label}-{stamp}"))
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}
