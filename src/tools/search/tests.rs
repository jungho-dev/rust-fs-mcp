//! tests.rs
//! tools::search::tests
//!
//! End-to-end fs-search handler tests over temp trees outside target/.
//!

use super::*;

fn temp_dir(prefix: &str) -> std::path::PathBuf {
    // target/ 아래를 피해야 default_excludes(=heavy dir 판정) 경로가 유지된다.
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
fn first_backend(result: &RawResult) -> String {
    result.structured.clone().unwrap()["results"][0]["data"]["backend"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn missing_pattern_is_error() {
    let result = handle_fs_search(&json!({ "items": [{ "path": "." }] }));
    assert!(result.is_error);
}

#[test]
fn line_range_args_get_targeted_hint() {
    let result =
        handle_fs_search(&json!({ "items": [{ "path": ".", "start_line": 3, "line_count": 2 }] }));
    assert!(result.is_error);
    let text = result.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("file-read-line-range"), "{text}");
}

#[test]
fn invalid_regex_falls_back_to_literal() {
    // "sendCancel(" 는 regex 파스 오류 → 리터럴 폴백 매치.
    let dir = temp_dir("rust-fs-mcp-litfb");
    std::fs::write(dir.join("sample.txt"), "call sendCancel( now\n").unwrap();

    let result = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "sendCancel(" }]
    }));
    assert!(!result.is_error, "{result:?}");
    let text = result.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("sendCancel("), "{text}");
    // 라벨에 파스 오류 요지가 실려 패턴 교정이 가능해야 한다.
    let backend = first_backend(&result);
    assert!(backend.contains("literal fallback"), "{result:?}");
    assert!(backend.contains("unclosed group"), "{backend}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn groups_lines_under_file_headings_with_context() {
    let dir = temp_dir("rust-fs-mcp-heading");
    std::fs::write(dir.join("a.txt"), "ctx before\nneedle hit\nctx after\n").unwrap();

    let result = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 1 }]
    }));
    assert!(!result.is_error, "{result:?}");
    let text = result.content[0]["text"].as_str().unwrap_or_default();
    // rg --heading 형태: 파일 경로 1줄 + "1-ctx", "2:hit", "3-ctx".
    assert!(text.contains("a.txt"), "{text}");
    assert!(text.contains("1-ctx before"), "{text}");
    assert!(text.contains("2:needle hit"), "{text}");
    assert!(text.contains("3-ctx after"), "{text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn default_excludes_skip_heavy_dirs() {
    let dir = temp_dir("rust-fs-mcp-heavy");
    std::fs::create_dir_all(dir.join("node_modules").join("pkg")).unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("node_modules").join("pkg").join("dep.js"),
        "needle\n",
    )
    .unwrap();
    std::fs::write(dir.join("target").join("out.txt"), "needle\n").unwrap();
    std::fs::write(dir.join("src").join("app.js"), "needle\n").unwrap();

    let result = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "needle" }]
    }));
    assert!(!result.is_error, "{result:?}");
    let text = result.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("app.js"), "{text}");
    assert!(!text.contains("dep.js"), "{text}");
    assert!(!text.contains("out.txt"), "{text}");

    // noDefaultExcludes:true 면 산출물 디렉터리도 검색된다.
    let all = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "needle", "noDefaultExcludes": true }]
    }));
    let text = all.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("dep.js"), "{text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn max_results_caps_match_and_context_lines() {
    let dir = temp_dir("rust-fs-mcp-maxres");
    let body: String = (1..=50)
        .map(|index| format!("needle line {index}\n"))
        .collect();
    std::fs::write(dir.join("many.txt"), body).unwrap();

    let result = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "needle", "maxResults": 5, "contextLines": 0 }]
    }));
    assert!(!result.is_error, "{result:?}");
    let structured = result.structured.clone().unwrap();
    // totalCount = 헤딩 1 + 콘텐츠 줄 5.
    assert_eq!(
        structured["results"][0]["data"]["totalCount"], 6,
        "{structured}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn single_file_root_searches_that_file() {
    let dir = temp_dir("rust-fs-mcp-singlefile");
    let file = dir.join("only.rs");
    std::fs::write(&file, "fn needle_here() {}\n").unwrap();

    let result = handle_fs_search(&json!({
        "items": [{ "path": file.display().to_string(), "pattern": "needle_here" }]
    }));
    assert!(!result.is_error, "{result:?}");
    let text = result.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("1:fn needle_here() {}"), "{text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn file_pattern_narrows_targets() {
    let dir = temp_dir("rust-fs-mcp-filepat");
    std::fs::write(dir.join("a.rs"), "needle\n").unwrap();
    std::fs::write(dir.join("b.js"), "needle\n").unwrap();

    let result = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "needle", "filePattern": "*.rs" }]
    }));
    assert!(!result.is_error, "{result:?}");
    let text = result.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("a.rs"), "{text}");
    assert!(!text.contains("b.js"), "{text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn lookaround_and_backreferences_match_via_fancy_engine() {
    let dir = temp_dir("rust-fs-mcp-fancy");
    std::fs::write(
        dir.join("a.txt"),
        "foo bar\nbaz bar\nhello hello\nhi there\n",
    )
    .unwrap();

    // lookbehind: "foo "가 앞선 "bar"만 매치되고 접두 텍스트는 결과에 실리지 않는다.
    let look = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "(?<=foo )bar", "contextLines": 0 }]
    }));
    assert!(!look.is_error, "{look:?}");
    let text = look.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("1:foo bar"), "{text}");
    assert!(!text.contains("2:baz bar"), "{text}");
    assert!(first_backend(&look).contains("fancy"), "{look:?}");

    // 부정형 lookbehind: "foo "가 앞서지 '않는' "bar"만 매치.
    let neg = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "(?<!foo )bar", "contextLines": 0 }]
    }));
    assert!(!neg.is_error, "{neg:?}");
    let text = neg.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("2:baz bar"), "{text}");
    assert!(!text.contains("1:foo bar"), "{text}");

    // backreference: 같은 단어가 반복된 줄만 매치.
    let backref = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": r"\b(\w+) \1\b", "contextLines": 0 }]
    }));
    assert!(!backref.is_error, "{backref:?}");
    let text = backref.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("3:hello hello"), "{text}");
    assert!(!text.contains("hi there"), "{text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn explicit_literal_matches_fixed_string_only() {
    let dir = temp_dir("rust-fs-mcp-literal");
    // "foo.bar"는 유효한 regex이기도 해서 자동 폴백이 없다: literal:true만이 정확한 계약.
    std::fs::write(dir.join("a.txt"), "foo.bar\nfooXbar\n").unwrap();

    let result = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "foo.bar", "literal": true, "contextLines": 0 }]
    }));
    assert!(!result.is_error, "{result:?}");
    let text = result.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("1:foo.bar"), "{text}");
    assert!(!text.contains("fooXbar"), "{text}");
    assert!(first_backend(&result).contains("(literal)"), "{result:?}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn word_match_bounds_ascii_and_hangul_words() {
    let dir = temp_dir("rust-fs-mcp-word");
    std::fs::write(
        dir.join("a.txt"),
        "cat\nconcatenate\n가나 이후\n가나다 이후\n",
    )
    .unwrap();

    let ascii = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "cat", "wordMatch": true, "contextLines": 0 }]
    }));
    assert!(!ascii.is_error, "{ascii:?}");
    let text = ascii.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("1:cat"), "{text}");
    assert!(!text.contains("concatenate"), "{text}");

    // Rust regex의 \b는 Unicode-aware: 완성형 음절 경계도 단어 경계로 성립한다.
    let hangul = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "가나", "wordMatch": true, "contextLines": 0 }]
    }));
    assert!(!hangul.is_error, "{hangul:?}");
    let text = hangul.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("3:가나 이후"), "{text}");
    assert!(!text.contains("가나다"), "{text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn multiline_lets_patterns_span_lines() {
    let dir = temp_dir("rust-fs-mcp-multiline");
    std::fs::write(
        dir.join("a.rs"),
        "alpha\nfn demo(\n  arg: u32,\n) -> bool {\nomega\n",
    )
    .unwrap();

    // 기본(라인 지향)에서는 `.`이 개행을 매치하지 않아 줄 경계를 넘는 매치가 없다.
    let single = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": r"fn demo\(.*?\) -> bool", "contextLines": 0 }]
    }));
    assert!(!single.is_error, "{single:?}");
    let text = single.content[0]["text"].as_str().unwrap_or_default();
    assert!(!text.contains("fn demo"), "{text}");

    let multi = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": r"fn demo\(.*?\) -> bool", "multiline": true, "contextLines": 0 }]
    }));
    assert!(!multi.is_error, "{multi:?}");
    let text = multi.content[0]["text"].as_str().unwrap_or_default();
    // 매치가 걸친 줄들이 실제 줄번호로 분해되어 나온다.
    assert!(text.contains("2:fn demo("), "{text}");
    assert!(text.contains("3:  arg: u32,"), "{text}");
    assert!(text.contains("4:) -> bool {"), "{text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn missing_root_is_an_error_not_zero_matches() {
    let missing = std::env::temp_dir().join("rust-fs-mcp-definitely-missing-root");
    let result = handle_fs_search(&json!({
        "items": [{ "path": missing.display().to_string(), "pattern": "x" }]
    }));
    assert!(result.is_error, "{result:?}");
    let text = result.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("Path does not exist"), "{text}");
}

#[test]
fn gitignore_is_respected_inside_a_repo_by_default() {
    let dir = temp_dir("rust-fs-mcp-gitignore");
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(init.success());
    std::fs::create_dir_all(dir.join("dist")).unwrap();
    std::fs::write(dir.join(".gitignore"), "dist/\n").unwrap();
    std::fs::write(dir.join("dist").join("bundle.js"), "needle\n").unwrap();
    std::fs::write(dir.join("app.js"), "needle\n").unwrap();

    let default = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 0 }]
    }));
    let text = default.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("app.js"), "{text}");
    assert!(!text.contains("bundle.js"), "{text}");

    let all = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 0, "respectGitignore": false }]
    }));
    let text = all.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("bundle.js"), "{text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn long_lines_are_clipped_around_the_match() {
    let dir = temp_dir("rust-fs-mcp-clip");
    std::fs::write(
        dir.join("min.js"),
        format!("{}needle{}\n", "a".repeat(100_000), "b".repeat(100_000)),
    )
    .unwrap();
    std::fs::write(dir.join("z.js"), "needle tail\n").unwrap();

    let result = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 0 }]
    }));
    assert!(!result.is_error, "{result:?}");
    let text = result.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("needle"), "{text}");
    assert!(text.contains("[line clipped: 200006 chars]"), "{text}");
    assert!(text.contains("1:needle tail"), "{text}");
    assert!(text.len() < 5_000, "{}", text.len());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn smart_case_applies_only_without_explicit_ignore_case() {
    let dir = temp_dir("rust-fs-mcp-smartcase");
    std::fs::write(dir.join("a.txt"), "Needle upper\nneedle lower\n").unwrap();
    let search = |pattern: &str, ignore_case: Option<bool>| {
        let mut item =
            json!({ "path": dir.display().to_string(), "pattern": pattern, "contextLines": 0 });
        if let Some(flag) = ignore_case {
            item["ignoreCase"] = json!(flag);
        }
        let result = handle_fs_search(&json!({ "items": [item] }));
        result.content[0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    let lower = search("needle", None);
    assert!(
        lower.contains("1:Needle upper") && lower.contains("2:needle lower"),
        "{lower}"
    );
    let upper = search("Needle", None);
    assert!(
        upper.contains("1:Needle upper") && !upper.contains("needle lower"),
        "{upper}"
    );
    let forced = search("Needle", Some(true));
    assert!(forced.contains("2:needle lower"), "{forced}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn output_budget_stops_collection_and_flags_limit_reached() {
    let dir = temp_dir("rust-fs-mcp-budget-stop");
    let body: String = (1..=20_000)
        .map(|index| format!("needle line {index}\n"))
        .collect();
    std::fs::write(dir.join("many.txt"), body).unwrap();

    let result = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 0 }]
    }));
    assert!(!result.is_error, "{result:?}");
    let data = &result.structured.as_ref().unwrap()["results"][0]["data"];
    assert_eq!(data["limitReached"], true, "{data}");
    assert!(data["totalCount"].as_u64().unwrap() < 20_000, "{data}");
    let text = result.content[0]["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("[stopped at maxResults or the output budget"),
        "{}",
        &text[text.len().saturating_sub(300)..]
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn results_are_sorted_by_path() {
    let dir = temp_dir("rust-fs-mcp-sorted");
    for name in ["c.txt", "a.txt", "b.txt"] {
        std::fs::write(dir.join(name), "needle\n").unwrap();
    }
    let result = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 0 }]
    }));
    let text = result.content[0]["text"].as_str().unwrap_or_default();
    let order: Vec<usize> = ["a.txt", "b.txt", "c.txt"]
        .iter()
        .map(|name| text.find(name).unwrap())
        .collect();
    assert!(order[0] < order[1] && order[1] < order[2], "{text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn euc_kr_files_are_searchable_and_readable() {
    let dir = temp_dir("rust-fs-mcp-euckr-search");
    // "한글 needle" in EUC-KR.
    std::fs::write(
        dir.join("legacy.txt"),
        [
            0xC7, 0xD1, 0xB1, 0xDB, b' ', b'n', b'e', b'e', b'd', b'l', b'e', b'\n',
        ],
    )
    .unwrap();

    let ascii = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "needle", "contextLines": 0 }]
    }));
    let text = ascii.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("1:한글 needle"), "{text}");

    let hangul = handle_fs_search(&json!({
        "items": [{ "path": dir.display().to_string(), "pattern": "한글", "contextLines": 0, "encoding": "euc-kr" }]
    }));
    assert!(!hangul.is_error, "{hangul:?}");
    let text = hangul.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("1:한글 needle"), "{text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

// 공개 dispatch 경유 단일 항목 검색
fn dispatch_search(item: Value) -> Value {
    crate::tools::dispatch_tool_call("fs-search", Some(json!({ "items": [item] })))
}
fn dispatch_status(response: &Value) -> &str {
    response["_meta"]["fsMcpResult"]["status"]
        .as_str()
        .unwrap_or_default()
}
fn dispatch_text(response: &Value) -> &str {
    response["structuredContent"]["data"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
}
fn dispatch_backend(response: &Value) -> &str {
    response["structuredContent"]["data"]["structuredContent"]["results"][0]["data"]["backend"]
        .as_str()
        .unwrap_or_default()
}

#[test]
fn word_match_treats_wrapper_only_valid_pattern_as_syntax_error() {
    let dir = temp_dir("rust-fs-mcp-word-unbalanced");
    std::fs::write(
        dir.join("a.txt"),
        "lit a)|(b end\nonly a here\nonly b here\n",
    )
    .unwrap();
    let path = dir.display().to_string();

    // 래퍼 없이도 동일한 리터럴 폴백
    let response = dispatch_search(
        json!({ "path": path, "pattern": "a)|(b", "wordMatch": true, "contextLines": 0 }),
    );
    assert_eq!(dispatch_status(&response), "success", "{response}");
    let text = dispatch_text(&response);
    assert!(text.contains("1:lit a)|(b end"), "{text}");
    assert!(!text.contains("only a here"), "{text}");
    assert!(!text.contains("only b here"), "{text}");
    let backend = dispatch_backend(&response);
    assert!(backend.contains("literal fallback"), "{backend}");
    assert!(backend.contains("unopened group"), "{backend}");

    // backtracking 경로도 패턴 단독 판정 (문법 오류 literal 폴백)
    for pattern in [r"(?<=x)a)|(b", r"(a)\1)|(b"] {
        let plain = dispatch_search(json!({ "path": path, "pattern": pattern }));
        let worded =
            dispatch_search(json!({ "path": path, "pattern": pattern, "wordMatch": true }));
        assert_eq!(dispatch_status(&plain), "success", "{plain}");
        assert_eq!(dispatch_status(&worded), "success", "{worded}");
        assert_eq!(dispatch_text(&plain), dispatch_text(&worded));
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn word_match_closes_extended_mode_comment_before_wrapper() {
    let dir = temp_dir("rust-fs-mcp-word-xcomment");
    std::fs::write(dir.join("a.txt"), "foo bar\nfood\n x foo\n x food\n").unwrap();
    let path = dir.display().to_string();

    let linear = dispatch_search(
        json!({ "path": path, "pattern": "(?x)foo # note", "wordMatch": true, "contextLines": 0 }),
    );
    assert_eq!(dispatch_status(&linear), "success", "{linear}");
    let text = dispatch_text(&linear);
    assert!(text.contains("1:foo bar"), "{text}");
    assert!(text.contains("3: x foo"), "{text}");
    assert!(!text.contains("food"), "{text}");
    assert_eq!(dispatch_backend(&linear), "native-grep");

    let fancy = dispatch_search(json!({
        "path": path,
        "pattern": r"(?x)(?<=x\s)foo # note",
        "wordMatch": true,
        "contextLines": 0
    }));
    assert_eq!(dispatch_status(&fancy), "success", "{fancy}");
    let text = dispatch_text(&fancy);
    assert!(text.contains("3: x foo"), "{text}");
    assert!(!text.contains("1:foo bar"), "{text}");
    assert!(!text.contains("food"), "{text}");
    assert!(dispatch_backend(&fancy).contains("fancy"), "{fancy}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unrepresentable_timeout_means_no_deadline() {
    let dir = temp_dir("rust-fs-mcp-timeout-max");
    std::fs::write(dir.join("a.txt"), "needle\n").unwrap();

    let response = dispatch_search(json!({
        "path": dir.display().to_string(),
        "pattern": "needle",
        "timeout_ms": u64::MAX,
        "contextLines": 0
    }));
    assert_eq!(dispatch_status(&response), "success", "{response}");
    assert!(dispatch_text(&response).contains("1:needle"), "{response}");

    std::fs::remove_dir_all(&dir).unwrap();
}
