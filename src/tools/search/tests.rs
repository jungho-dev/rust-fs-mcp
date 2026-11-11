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

// fast line path·강제 slow path 출력 비교
fn compare_line_paths(item: &Value, expects_fast: bool) -> usize {
    let Prepared {
        spec,
        matcher,
        backend,
    } = prepare_search(item, max_output_bytes() * 4 / 5)
        .unwrap()
        .unwrap();
    let SearchMatcher::Linear(adapter) = &matcher else {
        panic!("{item}: fancy matcher");
    };
    assert_eq!(adapter.line_bound, expects_fast, "{item}");
    let mut slow_adapter = adapter.clone();
    slow_adapter.line_bound = false;
    let outline = |search: Result<SearchSession, String>| {
        search.map(|session| {
            (
                session.lines,
                session.backend,
                session.partial,
                session.timed_out,
                session.limit_reached,
            )
        })
    };
    let fast = outline(finish_search(&spec, &matcher, backend.clone()));
    let slow = outline(finish_search(
        &spec,
        &SearchMatcher::Linear(slow_adapter),
        backend,
    ));
    assert_eq!(fast, slow, "{item}");
    fast.map_or(0, |(lines, ..)| lines.len())
}

#[test]
fn fast_line_path_output_equals_slow_path_output() {
    let dir = temp_dir("rust-fs-mcp-linepath");
    let lf = "needle at start\nends with needle\n\nfoo then bar\nbar then foo\n  indented word here\nWORD upper Needle\n한글 needle 줄\nnumbers 12345 end\ntrailing space  \nswordfish\n\nlast needle\n";
    let crlf = lf.replace('\n', "\r\n");
    let long = format!(
        "{}needle{}\nshort needle\n{}\n",
        "a".repeat(70_000),
        "b".repeat(70_000),
        "c".repeat(150_000)
    );
    let many: String = (1..=9_000)
        .map(|index| {
            if index % 97 == 0 {
                format!("row {index} needle word\r\n")
            } else {
                format!("row {index} filler\n")
            }
        })
        .collect();
    // EUC-KR 줄·줄 시작 비UTF-8 바이트
    let mut legacy = vec![0xC7, 0xD1, 0xB1, 0xDB];
    legacy.extend_from_slice(b" needle\r\n\xB0\xA1 foo word\n\xB0 foo\nword\xB0\n\xB0\xA1\r\n");
    let fixtures: [(&str, &[u8]); 10] = [
        ("lf.txt", lf.as_bytes()),
        ("crlf.txt", crlf.as_bytes()),
        ("lf-notail.txt", lf.trim_end().as_bytes()),
        ("crlf-notail.txt", crlf.trim_end().as_bytes()),
        ("cr-tail.txt", b"needle\r\nfoo word\r"),
        (
            "mixed.txt",
            b"needle\r\r\nfoo\rbar needle\n\r\nword\n\rneedle\r\n",
        ),
        ("long.txt", long.as_bytes()),
        ("many.txt", many.as_bytes()),
        ("legacy.txt", &legacy),
        ("binary.bin", b"needle before\nword\n\0needle after\n"),
    ];
    for (name, body) in fixtures {
        std::fs::write(dir.join(name), body).unwrap();
    }

    let cases = [
        (json!({ "pattern": "needle" }), true),
        (json!({ "pattern": "Needle" }), true),
        (json!({ "pattern": "foo|bar" }), true),
        (json!({ "pattern": "foo.*bar" }), true),
        (json!({ "pattern": "^needle" }), true),
        (json!({ "pattern": "needle$" }), true),
        (json!({ "pattern": "^$" }), true),
        (json!({ "pattern": "^" }), true),
        (json!({ "pattern": "$" }), true),
        (json!({ "pattern": "x*" }), true),
        (json!({ "pattern": r"\bword\b" }), true),
        (json!({ "pattern": r"\b{start}need" }), true),
        (json!({ "pattern": r"\w+" }), true),
        (json!({ "pattern": r"^\w+$" }), true),
        (json!({ "pattern": r"\d+" }), true),
        (json!({ "pattern": "한글" }), true),
        (json!({ "pattern": "NEEDLE", "ignoreCase": true }), true),
        (json!({ "pattern": "word", "wordMatch": true }), true),
        (json!({ "pattern": "12345 end", "literal": true }), true),
        (json!({ "pattern": "needle", "maxResults": 3 }), true),
        (json!({ "pattern": r"\s+$" }), false),
        (json!({ "pattern": r"needle\s" }), false),
        (json!({ "pattern": "[^a-z]" }), false),
        (json!({ "pattern": r"\Bord" }), false),
        (json!({ "pattern": r"[a-z]+ed\b" }), false),
        (json!({ "pattern": r"(?-u:[a-z]+ed\b)" }), true),
        (json!({ "pattern": r"\b{start-half}need" }), false),
        (json!({ "pattern": r"\Aneedle" }), false),
        (json!({ "pattern": r"needle\z" }), false),
        (json!({ "pattern": "(?-m)^needle" }), false),
        (json!({ "pattern": "(?-R)needle$" }), false),
        (json!({ "pattern": "(?s)foo.bar" }), false),
        (json!({ "pattern": r"\r" }), false),
        (json!({ "pattern": r"needle\n" }), false),
        (json!({ "pattern": "needle", "multiline": true }), false),
    ];
    let mut total = 0;
    for (case, expects_fast) in cases {
        for context in [0, 2] {
            let mut item = case.clone();
            item["path"] = json!(dir.display().to_string());
            item["contextLines"] = json!(context);
            total += compare_line_paths(&item, expects_fast);
        }
    }
    assert!(total > 1_000, "{total}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn crlf_anchors_match_line_edges_like_lf() {
    let dir = temp_dir("rust-fs-mcp-crlf-anchor");
    let lf = "on\noff\n\nlast off\n";
    std::fs::write(dir.join("lf.txt"), lf).unwrap();
    std::fs::write(dir.join("crlf.txt"), lf.replace('\n', "\r\n")).unwrap();
    let search = |name: &str, pattern: &str, multiline: bool| {
        let result = regex_item(&json!({
            "path": dir.join(name).display().to_string(),
            "pattern": pattern,
            "multiline": multiline,
            "contextLines": 0
        }));
        assert!(!result.is_error, "{name} {pattern}: {result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        // 헤딩 제외 매치 줄
        text.lines().skip(1).map(str::to_string).collect::<Vec<_>>()
    };

    for name in ["lf.txt", "crlf.txt"] {
        for multiline in [false, true] {
            assert_eq!(
                search(name, "off$", multiline),
                ["2:off", "4:last off"],
                "{name}"
            );
            assert_eq!(search(name, "^off", multiline), ["2:off"], "{name}");
            assert_eq!(search(name, "^$", multiline), ["3:"], "{name}");
        }
        // `.` CR 제외
        assert_eq!(search(name, "^..$", false), ["1:on"], "{name}");
        assert!(search(name, "off.", false).is_empty(), "{name}");
        assert_eq!(search(name, r"^\s*$", false), ["3:"], "{name}");
        // backtracking 엔진 동일 앵커
        assert_eq!(
            search(name, "(?<=l)ast off$", false),
            ["4:last off"],
            "{name}"
        );
        assert_eq!(
            search(name, "(?<=o)n$|^(?=l)", false),
            ["1:on", "4:last off"]
        );
        // 줄 단위 검색의 종결 CR 비노출
        assert!(search(name, r"\r", false).is_empty(), "{name}");
    }
    assert_eq!(
        search("crlf.txt", r"off\r\n", true),
        ["2:off", "4:last off"]
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn file_root_output_equals_directory_walk_output() {
    let dir = temp_dir("rust-fs-mcp-fileroot");
    let mut wide = vec![0xFF, 0xFE];
    wide.extend(
        "needle 한글\r\nnext needle\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    let fixtures: [(&str, &[u8]); 5] = [
        (
            "plain.txt",
            b"ctx one\nneedle two\nctx three\nneedle four\n",
        ),
        (
            "legacy.txt",
            &[
                0xC7, 0xD1, 0xB1, 0xDB, b' ', b'n', b'e', b'e', b'd', b'l', b'e', b'\n',
            ],
        ),
        ("binary.bin", b"needle head\n\0needle tail\n"),
        ("wide.txt", &wide),
        ("empty.txt", b""),
    ];
    for (name, body) in fixtures {
        std::fs::write(dir.join(name), body).unwrap();
    }

    let variants = [
        json!({ "pattern": "needle" }),
        json!({ "pattern": "needle", "contextLines": 0 }),
        json!({ "pattern": "needle", "maxResults": 1 }),
        json!({ "pattern": "needle", "encoding": "euc-kr" }),
        json!({ "pattern": "needle", "multiline": true }),
        json!({ "pattern": "needle", "timeout_ms": 0 }),
        json!({ "pattern": "(?<=need)le" }),
        json!({ "pattern": "absent" }),
    ];
    let mut matched = 0;
    for (name, _) in fixtures {
        for variant in &variants {
            let mut file_item = variant.clone();
            file_item["path"] = json!(dir.join(name).display().to_string());
            let mut walk_item = variant.clone();
            walk_item["path"] = json!(dir.display().to_string());
            walk_item["filePattern"] = json!(name);
            let direct = regex_item(&file_item);
            let walked = regex_item(&walk_item);
            assert_eq!(direct.content, walked.content, "{name} {variant}");
            assert_eq!(direct.structured, walked.structured, "{name} {variant}");
            assert_eq!(direct.is_error, walked.is_error, "{name} {variant}");
            assert_eq!(direct.meta, walked.meta, "{name} {variant}");
            let text = direct.content[0]["text"].as_str().unwrap_or_default();
            matched += usize::from(text.contains("needle") && !direct.is_error);
        }
    }
    assert!(matched >= 15, "{matched}");

    // 파일 루트에서도 filePattern 검증 유지
    let invalid = handle_fs_search(&json!({ "items": [{
        "path": dir.join("plain.txt").display().to_string(),
        "pattern": "needle",
        "filePattern": "{"
    }] }));
    assert!(invalid.is_error, "{invalid:?}");
    let text = invalid.content[0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("Invalid filePattern glob"), "{text}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn default_excludes_skip_only_directories_by_exact_name() {
    let dir = temp_dir("rust-fs-mcp-heavy-names");
    for relative in [
        "src/app.js",
        "node_modules/pkg/dep.js",
        "pkg/target/out.txt",
        "pkg/target/deep/inner.txt",
        "other/Target/upper.txt",
        "docs/target",
        ".git/config",
        ".hidden/dot.txt",
    ] {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "needle\n").unwrap();
    }
    let named_file = format!("docs{}target", std::path::MAIN_SEPARATOR);
    let search = |root: &std::path::Path, extra: Value| {
        let mut item = extra;
        item["path"] = json!(root.display().to_string());
        item["pattern"] = json!("needle");
        item["contextLines"] = json!(0);
        let result = handle_fs_search(&json!({ "items": [item] }));
        assert!(!result.is_error, "{result:?}");
        let text = result.content[0]["text"].as_str().unwrap_or_default();
        [
            "app.js",
            "dep.js",
            "out.txt",
            "inner.txt",
            "upper.txt",
            named_file.as_str(),
            "config",
            "dot.txt",
        ]
        .into_iter()
        .filter(|name| text.contains(name))
        .collect::<Vec<_>>()
    };

    assert_eq!(
        search(&dir, json!({})),
        ["app.js", "upper.txt", named_file.as_str()]
    );
    assert_eq!(
        search(&dir, json!({ "includeHidden": true })),
        ["app.js", "upper.txt", named_file.as_str(), "dot.txt"]
    );
    assert_eq!(
        search(&dir, json!({ "noDefaultExcludes": true })),
        [
            "app.js",
            "dep.js",
            "out.txt",
            "inner.txt",
            "upper.txt",
            named_file.as_str()
        ]
    );
    assert_eq!(
        search(
            &dir,
            json!({ "includeHidden": true, "noDefaultExcludes": true })
        )
        .len(),
        8
    );
    assert_eq!(search(&dir, json!({ "filePattern": "*.js" })), ["app.js"]);
    // 제외 디렉터리 내부 루트는 그대로 검색
    assert_eq!(search(&dir.join("node_modules"), json!({})), ["dep.js"]);
    assert_eq!(
        search(&dir.join("pkg").join("target"), json!({})),
        ["out.txt", "inner.txt"]
    );

    std::fs::remove_dir_all(&dir).unwrap();
}
