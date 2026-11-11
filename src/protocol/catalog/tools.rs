//! tools.rs
//! protocol::catalog::tools
//!
//! Single source of truth for the public tool catalog (name, description, annotation, JSON input schema) exposed by tools/list.
//! Caches the fixed full catalog in OnceLock.
//! Marks the fixed always-load tool set with _meta {"anthropic/alwaysLoad": true} so
//! schema-deferring hosts expose those tools upfront.
//! Declares _meta {"anthropic/maxResultSizeChars"} from the server output budget so the host
//! keeps results inline instead of saving them to a file.
//!

use crate::core::client::ClientHost;
use crate::core::response::host_output_bytes;
use crate::core::settings::settings;
use serde_json::{Map, Value, json};
use std::sync::OnceLock;

static FULL_TOOL_CATALOG: OnceLock<Vec<Value>> = OnceLock::new();
static WIRE_TOOLS_JSON: OnceLock<String> = OnceLock::new();
// Claude Code 허용 상한
const MAX_RESULT_SIZE_CHARS: usize = 500_000;
const ALWAYS_LOAD: &[&str] = &[
    "file-read",
    "fs-search",
    "path-find",
    "file-edit-lines",
    "file-edit",
    "git-status",
    "git-diff",
    "dir-list",
    "file-read-line-range",
    "path-stat",
    "git-show",
];

// 1. Tool catalog -------------------------------------------------------------
pub fn tool_catalog() -> Vec<Value> {
    full_tool_catalog_ref().clone()
}
// The tools/list wire body `{"tools":[...]}` is serialized exactly once per process and the
// server splices the cached bytes into the response envelope verbatim — zero re-serialization
// (and zero catalog clone) on the warm path, mirroring go-fs-mcp WireTools.
pub fn wire_tools_json() -> &'static str {
    WIRE_TOOLS_JSON.get_or_init(|| {
        let mut out = String::with_capacity(32 * 1024);
        out.push_str("{\"tools\":");
        match serde_json::to_string(full_tool_catalog_ref()) {
            Ok(tools) => out.push_str(&tools),
            Err(_) => out.push_str("[]"),
        }
        out.push('}');
        out
    })
}
fn full_tool_catalog_ref() -> &'static Vec<Value> {
    FULL_TOOL_CATALOG.get_or_init(|| build_tool_catalog(settings().mysql.is_some()))
}
// mysql-query는 [mysql] 설정 시에만 노출
fn build_tool_catalog(with_mysql: bool) -> Vec<Value> {
    let mut tools = vec![
        tool(
            "file-read",
            "Read Files",
            "Read files in parallel; put every file in one paths[] (or items) call.\nUse items for offset, length, or URL reads. A truncated item ends with next: offset=N; pass that offset to continue.\nUTF-16 (BOM) and EUC-KR/CP949 text is decoded automatically and reported as encoding.",
            read_schema(),
            true,
            None,
            Some(true),
        ),
        tool(
            "file-read-line-range",
            "Read Line Ranges",
            "Read line ranges from local text files; each line carries its 1-based number.\nUse paths for whole files or items with start_line and line_count. A truncated item ends with next: start_line=N; pass that start_line to continue.",
            line_range_schema(),
            true,
            None,
            Some(false),
        ),
        tool(
            "file-write",
            "Write Files",
            "Write files in parallel. Prefer content_path for large text.",
            write_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "dir-create",
            "Create Directories",
            "Create directories in parallel.",
            dir_create_schema(),
            false,
            Some(false),
            Some(false),
        ),
        tool(
            "dir-list",
            "List Directories",
            "List directories in parallel. Skips node_modules/, target/, and .git/ unless the listed path is inside one.",
            dir_list_schema(),
            true,
            None,
            Some(false),
        ),
        tool(
            "path-copy",
            "Copy Paths",
            "Copy files or directories in parallel.",
            copy_schema(),
            false,
            Some(false),
            Some(false),
        ),
        tool(
            "path-move",
            "Move Paths",
            "Move or rename files in parallel.",
            move_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "path-remove",
            "Remove Paths",
            "Delete files or directories in parallel. Use paths[] with shared recursive/force, or items[] for per-path flags.",
            remove_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "fs-search",
            "Search File Contents",
            "Ripgrep-compatible regex content search; prefer over shell rg.\nRust regex syntax with Unicode-aware \\d \\w \\b (Hangul word boundaries work); look-around and backreferences switch to fancy-regex, run in a kill-able worker process stopped at timeout_ms + 1 s; an invalid regex, including a fancy-regex syntax error, falls back to literal search. ^ and $ treat CRLF as one line ending; matching \\r itself needs multiline:true.\nLike rg: smart-case, .gitignore respected (not global excludes or .git/info/exclude), node_modules/ and target/ skipped unless the root is inside one.\nResults are path-sorted; limitReached:true or timedOut:true marks partial results. A missing path is an error.",
            search_regex_schema(),
            true,
            None,
            Some(false),
        ),
        tool(
            "path-find",
            "Find Paths",
            "Find files and directories by name regex or glob, type, extension, size, and modification time (fd semantics) without reading contents.\nLike fs-search: smart-case, .gitignore respected, hidden entries and node_modules/, target/, .git/ skipped unless includeHidden or noDefaultExcludes.\nEntries are root-relative (directories end with /) and path-sorted; limitReached:true or timedOut:true marks partial results.",
            path_find_schema(),
            true,
            None,
            Some(false),
        ),
        tool(
            "ast-search",
            "Search Code Structure",
            "Structural code search with ast-grep patterns ($VAR, $$$ARGS) over files or directories; matches syntax nodes, never comments or strings.\nrewrite previews the replacement as a unified diff without writing; apply it with file-replace mode:\"ast\".\nLines and columns are 1-based; .gitignore is respected like fs-search and node_modules/, target/ are skipped. ast-grep parses UTF-8 only, so EUC-KR and UTF-16 files are skipped (an explicit single file errors).
Requires the ast-grep CLI (RUST_FS_MCP_AST_GREP, beside the server executable on Windows, on PATH, or through an npm install); lang is inferred from file extensions when omitted. A timeout returns the matches found so far with timedOut:true.",
            ast_search_schema(),
            true,
            None,
            Some(false),
        ),
        tool(
            "path-stat",
            "Path Metadata",
            "Retrieve metadata for filesystem paths in parallel.",
            infos_schema(),
            true,
            None,
            Some(false),
        ),
        tool(
            "file-edit",
            "Replace Text Blocks",
            "Apply exact block replacements in parallel. old_string must match exactly expected_replacements times; otherwise the edit fails with the matching line numbers and changes nothing. dryRun:true returns the unified diff without writing. Prefer *_path for large text.",
            edit_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "file-edit-lines",
            "Edit File Lines",
            "Replace, insert, or delete by 1-based line numbers; prefer over file-edit when line numbers are known. EOL is auto-detected. after:true inserts after end_line without removing it. Each result returns total_lines; use it as the basis for follow-up edits. dryRun:true returns the unified diff without writing.",
            edit_lines_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "file-replace",
            "Replace by Pattern",
            "Regex or literal find-and-replace inside files (sd semantics): $1 and ${name} expand captures, an ambiguous $1a is rejected, and an invalid regex errors instead of falling back.\nEvery item is validated and encoded before any file is written, so one failing item writes nothing; expectedReplacements guards the match count. Each item has a timeout_ms budget (default 25 s) and a 256 MiB result cap.\ndryRun:true returns the unified diff without writing; applied edits return it too. mode:\"ast\" rewrites by ast-grep pattern (same ast-grep CLI lookup as ast-search).",
            replace_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "file-diff",
            "Compare Files",
            "Compare two local files, or a file against inline text, as a git-style unified diff without git.\nCRLF and LF are folded before comparing and reported as eolChanged; ignoreWhitespace mirrors git diff -b.",
            file_diff_schema(),
            true,
            None,
            Some(false),
        ),
        tool(
            "git-add",
            "Stage Git Files",
            "Stage files for a repository path; pass every file in one paths[] call.",
            git_add_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "git-amend",
            "Amend Git Commit",
            "Amend the last commit in a repository path.\nOmit message to keep it (--no-edit). filesToStage amends ONLY those files (git commit --only; existing staged entries stay staged); resetAuthor resets authorship.\nThe commit timestamp refreshes to the amend time.",
            git_amend_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "git-commit",
            "Create Git Commit",
            "Create a commit in a repository path.\nMessage: multi-line Conventional Commit (lowercase English type; the summary may be any language).\n<type>: <summary>\n- <change detail>\n- <verification or behavior detail>\nUse messagePath for long messages. filesToStage commits ONLY those files (git commit --only; existing staged entries stay staged; untracked files are included).",
            git_commit_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "git-diff",
            "Show Git Diff",
            "Show differences for a repository path. paths[] scopes to files, nameOnly lists changed files, check flags whitespace errors and conflict markers.",
            git_diff_schema(),
            true,
            None,
            Some(false),
        ),
        tool(
            "git-show",
            "Show Git Objects",
            "Show git objects or file content for a repository path. objects[] fetches several revisions in one call; stat:true returns a diffstat.",
            git_show_schema(),
            true,
            None,
            Some(false),
        ),
        tool(
            "git-status",
            "Show Git Status",
            "Show working tree status, staging, and conflicts for a repository path.",
            git_status_schema(),
            true,
            None,
            Some(false),
        ),
        tool(
            "web-fetch",
            "Fetch URLs",
            "Fetch URLs over HTTP/HTTPS (no browser) as markdown, text, links, readability, or raw html. SSRF-guarded, decodes non-UTF-8 charsets, no summarizer pass.\nTier 1 for static or server-rendered pages and JSON endpoints; client-rendered (JS/SPA) pages are not supported.\nPage long text with offset/maxChars; a cut body ends with next offset=N. Private/link-local hosts are blocked; localhost is allowed.",
            web_fetch_schema(),
            true,
            None,
            Some(true),
        ),
        tool(
            "web-extract",
            "Extract HTML Content",
            "Convert held HTML (inline html or a local file path) into markdown, text, links, or readability content without network access. baseUrl resolves relative links.",
            web_extract_schema(),
            true,
            None,
            Some(false),
        ),
        tool(
            "download-to-file",
            "Download Files",
            "Download URLs to local files over HTTP/HTTPS through a temp file and rename, so partial downloads never land on the destination. Private/link-local hosts are blocked; localhost is allowed. overwrite:true replaces an existing file.",
            download_schema(),
            false,
            Some(true),
            Some(true),
        ),
    ];
    if with_mysql {
        tools.push(tool(
            "mysql-query",
            "Query MySQL",
            "Run one read-only SQL statement against the MySQL server set in the [mysql] section of config.toml.\nAllowed: SELECT, SHOW, DESCRIBE, EXPLAIN, and WITH ... SELECT. Writes, DDL, multiple statements, locking reads, SLEEP/BENCHMARK, EXPLAIN ANALYZE, and INTO OUTFILE are rejected, and every statement runs in a READ ONLY transaction under a time limit (the server stops a SELECT, the client cancels anything else).\nRows return as one JSON object per line (DECIMAL and dates as strings); calls run one at a time. Read the schema first (SHOW, DESCRIBE, information_schema filtered by TABLE_SCHEMA and TABLE_NAME) and keep every SELECT small with LIMIT.",
            mysql_query_schema(),
            true,
            None,
            Some(false),
        ));
    }
    // 서버 예산 = 클라이언트 상한 (파일 덤프 방지)
    let result_chars = host_output_bytes(ClientHost::ClaudeCode).min(MAX_RESULT_SIZE_CHARS);
    for tool in tools.iter_mut() {
        let name = tool["name"].as_str().unwrap_or("");
        let mut meta = Map::new();
        if ALWAYS_LOAD.contains(&name) {
            meta.insert("anthropic/alwaysLoad".to_string(), json!(true));
        }
        meta.insert(
            "anthropic/maxResultSizeChars".to_string(),
            json!(result_chars),
        );
        tool["_meta"] = Value::Object(meta);
    }
    tools
}
// 2. Tool entry ---------------------------------------------------------------
fn tool(
    name: &str,
    title: &str,
    description: &str,
    input_schema: Value,
    read_only: bool,
    destructive: Option<bool>,
    open_world: Option<bool>,
) -> Value {
    let mut annotations = Map::new();
    annotations.insert("title".to_string(), json!(title));
    annotations.insert("readOnlyHint".to_string(), json!(read_only));
    if let Some(value) = destructive {
        annotations.insert("destructiveHint".to_string(), json!(value));
    }
    if let Some(value) = open_world {
        annotations.insert("openWorldHint".to_string(), json!(value));
    }
    json!({
      "name": name,
      "title": title,
      "description": description,
      "inputSchema": input_schema,
      "annotations": annotations
    })
}
// 3. Schema helpers -----------------------------------------------------------
fn object_schema(properties: Map<String, Value>, required: Vec<&str>) -> Value {
    let mut props = properties;
    props.insert(
        "args_path".to_string(),
        json!({"type": "string", "description": "UTF-8 JSON file holding the complete arguments."}),
    );
    props.insert("args_offset".to_string(), number_default(0));
    props.insert("args_length".to_string(), number());
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), Value::Object(props));
    if !required.is_empty() {
        schema.insert("required".to_string(), json!(required));
    }
    schema.insert("additionalProperties".to_string(), json!(false));
    Value::Object(schema)
}
fn item_object(properties: Map<String, Value>, required: Vec<&str>) -> Value {
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_string(), json!(required));
    }
    schema.insert("additionalProperties".to_string(), json!(false));
    Value::Object(schema)
}
fn array_of(item: Value) -> Value {
    json!({"type": "array", "items": item, "minItems": 1})
}
fn prop(entries: Vec<(&str, Value)>) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}
fn string() -> Value {
    json!({"type": "string"})
}
fn number() -> Value {
    json!({"type": "number"})
}
fn integer_min(value: i64) -> Value {
    json!({"type": "integer", "minimum": value})
}
fn boolean() -> Value {
    json!({"type": "boolean"})
}
fn boolean_default(value: bool) -> Value {
    json!({"type": "boolean", "default": value})
}
fn number_default(value: i64) -> Value {
    json!({"type": "number", "default": value})
}
fn string_array() -> Value {
    json!({"type": "array", "items": {"type": "string"}})
}
fn string_array_min() -> Value {
    json!({"type": "array", "items": {"type": "string"}, "minItems": 1})
}
fn enum_str(values: &[&str], default: Option<&str>) -> Value {
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("string"));
    schema.insert("enum".to_string(), json!(values));
    if let Some(default) = default {
        schema.insert("default".to_string(), json!(default));
    }
    Value::Object(schema)
}
fn dry_run() -> Value {
    json!({
      "type":
        "boolean",
      "default": false,
      "description":
        "Return the unified diff and counts without writing the file. Each item is previewed against the file on disk, so several items on one file are not chained."
    })
}
fn allow_missing() -> Value {
    json!({
      "type":
        "boolean",
      "default": false,
      "description":
        "Return missing local paths as non-error missing results."
    })
}
// 4. Public schemas -----------------------------------------------------------
fn read_item_schema() -> Value {
    item_object(
        prop(vec![
            ("path", string()),
            ("isUrl", boolean_default(false)),
            ("offset", number_default(0)),
            ("length", number()),
        ]),
        vec!["path"],
    )
}
fn read_schema() -> Value {
    object_schema(
        prop(vec![
            ("allowMissing", allow_missing()),
            ("paths", string_array_min()),
            ("items", array_of(read_item_schema())),
        ]),
        vec![],
    )
}
fn line_range_item_schema() -> Value {
    item_object(
        prop(vec![
            ("path", string()),
            (
                "start_line",
                json!({
                  "type":
                    "integer",
                  "minimum": 1,
                  "default": 1,
                  "description":
                    "First line to return, using 1-based line numbers."
                }),
            ),
            (
                "line_count",
                json!({
                  "type":
                    "integer",
                  "minimum": 1,
                  "description":
                    "Maximum number of lines to return. Omit to read through end of file."
                }),
            ),
        ]),
        vec!["path"],
    )
}
fn line_range_schema() -> Value {
    object_schema(
        prop(vec![
            ("allowMissing", allow_missing()),
            ("paths", string_array_min()),
            ("items", array_of(line_range_item_schema())),
        ]),
        vec![],
    )
}
fn write_item_schema() -> Value {
    item_object(
        prop(vec![
            ("path", string()),
            (
                "content_path",
                json!({"type": "string", "description": "Read UTF-8 content from this file. Preferred for large generated or pasted text."}),
            ),
            (
                "content",
                json!({"type": "string", "description": "Inline text accepted. For very large generated or pasted payloads, content_path or args_path can still reduce transport overhead."}),
            ),
            ("content_offset", number_default(0)),
            ("content_length", number()),
            (
                "mode",
                json!({"type": "string", "enum": ["rewrite", "append"], "default": "rewrite", "description": "rewrite (default) replaces the file; append adds to its end."}),
            ),
        ]),
        vec!["path"],
    )
}
fn write_schema() -> Value {
    object_schema(
        prop(vec![("items", array_of(write_item_schema()))]),
        vec!["items"],
    )
}
fn dir_create_schema() -> Value {
    object_schema(prop(vec![("paths", string_array_min())]), vec!["paths"])
}
fn dir_item_schema() -> Value {
    item_object(
        prop(vec![
            ("path", string()),
            (
                "depth",
                json!({"type": "number", "default": 2, "description": "Directory levels to list; default 2."}),
            ),
            ("maxEntries", integer_min(1)),
            (
                "excludePatterns",
                json!({"type": "array", "items": {"type": "string"}, "default": []}),
            ),
            ("includeFiles", boolean_default(true)),
            (
                "noDefaultExcludes",
                json!({"type": "boolean", "default": false, "description": "Set true to also list node_modules/, target/, and .git/ (hidden by default)."}),
            ),
        ]),
        vec!["path"],
    )
}
fn dir_list_schema() -> Value {
    object_schema(
        prop(vec![
            ("allowMissing", allow_missing()),
            ("items", array_of(dir_item_schema())),
        ]),
        vec!["items"],
    )
}
fn copy_schema() -> Value {
    object_schema(
        prop(vec![(
            "items",
            array_of(item_object(
                prop(vec![
                    ("source", string()),
                    ("destination", string()),
                    ("recursive", boolean_default(false)),
                    ("force", boolean_default(false)),
                ]),
                vec!["source", "destination"],
            )),
        )]),
        vec!["items"],
    )
}
fn move_schema() -> Value {
    object_schema(
        prop(vec![(
            "items",
            array_of(item_object(
                prop(vec![("source", string()), ("destination", string())]),
                vec!["source", "destination"],
            )),
        )]),
        vec!["items"],
    )
}
fn remove_schema() -> Value {
    // paths[](간단 삭제) 또는 items[](항목별 플래그) 중 하나를 받는다(둘 다 선택, 핸들러가 존재 검증).
    object_schema(
        prop(vec![
            ("paths", string_array_min()),
            ("recursive", boolean_default(false)),
            ("force", boolean_default(false)),
            (
                "items",
                array_of(item_object(
                    prop(vec![
                        ("path", string()),
                        ("recursive", boolean_default(false)),
                        ("force", boolean_default(false)),
                    ]),
                    vec!["path"],
                )),
            ),
        ]),
        vec![],
    )
}
fn search_regex_item_schema() -> Value {
    item_object(
        prop(vec![
            ("path", string()),
            ("pattern", string()),
            ("pattern_path", string()),
            ("pattern_offset", number_default(0)),
            ("pattern_length", number()),
            ("filePattern", string()),
            (
                "ignoreCase",
                json!({
                  "type":
                    "boolean",
                  "description":
                    "Omit for smart-case (case-insensitive unless the pattern has an uppercase letter); true or false forces it."
                }),
            ),
            (
                "respectGitignore",
                json!({
                  "type":
                    "boolean",
                  "default": true,
                  "description":
                    "Skip files ignored by repository .gitignore files. Set false to search ignored build output."
                }),
            ),
            (
                "maxColumns",
                json!({
                  "type":
                    "integer",
                  "minimum": 0,
                  "default": 500,
                  "description":
                    "Clip longer lines to this many characters around the match; default 500, 0 disables clipping."
                }),
            ),
            (
                "encoding",
                json!({
                  "type":
                    "string",
                  "description":
                    "Transcode files from this encoding label before matching (e.g. euc-kr, utf-16le)."
                }),
            ),
            (
                "literal",
                json!({
                  "type":
                    "boolean",
                  "default": false,
                  "description":
                    "Treat pattern as a fixed string instead of a regex (rg -F); no metacharacter escaping needed."
                }),
            ),
            (
                "wordMatch",
                json!({
                  "type":
                    "boolean",
                  "default": false,
                  "description":
                    "Match only at word boundaries (rg -w). Boundaries are Unicode-aware, so Hangul words bound correctly."
                }),
            ),
            (
                "multiline",
                json!({
                  "type":
                    "boolean",
                  "default": false,
                  "description":
                    "Let the pattern span line boundaries and make . match newlines (rg -U + dot-all). Reads each searched file fully into memory."
                }),
            ),
            (
                "maxResults",
                json!({
                  "type":
                    "number",
                  "description":
                    "Cap on emitted match+context lines combined (not match count). With contextLines=2 one hit can emit up to 5 lines."
                }),
            ),
            ("includeHidden", boolean_default(false)),
            (
                "noDefaultExcludes",
                json!({
                  "type":
                    "boolean",
                  "default": false,
                  "description":
                    "Set true to also search **/node_modules/** and **/target/** (skipped by default for speed)."
                }),
            ),
            (
                "contextLines",
                json!({"type": "number", "default": 2, "description": "Lines of context around each match; default 2."}),
            ),
            ("timeout_ms", number_default(25000)),
        ]),
        vec!["path"],
    )
}
fn search_regex_schema() -> Value {
    object_schema(
        prop(vec![("items", array_of(search_regex_item_schema()))]),
        vec!["items"],
    )
}
fn infos_schema() -> Value {
    object_schema(
        prop(vec![
            ("allowMissing", allow_missing()),
            ("paths", string_array_min()),
        ]),
        vec!["paths"],
    )
}
fn edit_lines_schema() -> Value {
    object_schema(
        prop(vec![(
            "items",
            array_of(item_object(
                prop(vec![
                    ("file_path", string()),
                    ("start_line", integer_min(1)),
                    ("end_line", integer_min(1)),
                    ("replacement", string()),
                    ("replacement_path", string()),
                    ("replacement_offset", number_default(0)),
                    ("replacement_length", number()),
                    ("after", boolean_default(false)),
                    (
                        "expected_lines",
                        json!({
                          "type":
                            "number",
                          "description":
                            "Optional drift guard: expected total line count of the WHOLE file before the edit. A value equal to the replaced range length (end_line-start_line+1) is also accepted."
                        }),
                    ),
                    ("dryRun", dry_run()),
                ]),
                vec!["file_path", "start_line"],
            )),
        )]),
        vec!["items"],
    )
}
fn edit_schema() -> Value {
    let expected = json!({"type": "integer", "minimum": 1, "default": 1, "description": "Exact number of matches to replace. Omitted means 1, so an ambiguous old_string fails instead of replacing every match."});
    object_schema(
        prop(vec![(
            "items",
            array_of(item_object(
                prop(vec![
                    ("file_path", string()),
                    ("old_string", string()),
                    ("old_string_path", string()),
                    ("old_string_offset", number_default(0)),
                    ("old_string_length", number()),
                    ("new_string", string()),
                    ("new_string_path", string()),
                    ("new_string_offset", number_default(0)),
                    ("new_string_length", number()),
                    ("expected_replacements", expected),
                    ("dryRun", dry_run()),
                ]),
                vec!["file_path"],
            )),
        )]),
        vec!["items"],
    )
}
fn replace_schema() -> Value {
    object_schema(
        prop(vec![(
            "items",
            array_of(item_object(
                prop(vec![
                    ("file_path", string()),
                    ("pattern", string()),
                    ("pattern_path", string()),
                    ("pattern_offset", number_default(0)),
                    ("pattern_length", number()),
                    (
                        "replacement",
                        json!({"type": "string", "description": "Replacement text; \"\" deletes the match. Regex mode expands $1, ${1}, ${name}, and $$; literal and ast modes differ (ast uses ast-grep $VAR metavariables)."}),
                    ),
                    ("replacement_path", string()),
                    ("replacement_offset", number_default(0)),
                    ("replacement_length", number()),
                    (
                        "mode",
                        json!({"type": "string", "enum": ["regex", "literal", "ast"], "default": "regex", "description": "regex (Rust syntax; look-around and backreferences switch to fancy-regex, \\K inside look-around is rejected, fancy items run in a kill-able worker stopped at timeout_ms), literal (fixed string), or ast (ast-grep pattern and rewrite; UTF-8 files without BOM only, no ignoreCase/dotAll/wordMatch, nested matches keep the outermost like ast-grep -U)."}),
                    ),
                    (
                        "ignoreCase",
                        json!({"type": "boolean", "default": false, "description": "Case-insensitive match. Unlike fs-search, omitted means case-sensitive."}),
                    ),
                    (
                        "dotAll",
                        json!({"type": "boolean", "default": false, "description": "Let . match newlines (sd s flag); ^ and $ always match at line boundaries."}),
                    ),
                    ("wordMatch", boolean_default(false)),
                    (
                        "maxReplacements",
                        json!({"type": "integer", "minimum": 0, "default": 0, "description": "Replace at most this many matches in file order (sd -n); 0 replaces every match."}),
                    ),
                    (
                        "expectedReplacements",
                        json!({"type": "integer", "minimum": 0, "description": "Fail without writing anything unless exactly this many replacements happen."}),
                    ),
                    (
                        "lang",
                        json!({"type": "string", "description": "ast mode language (ast-grep --lang); inferred from the file extension when omitted."}),
                    ),
                    (
                        "timeout_ms",
                        json!({"type": "number", "default": 25000, "description": "Time budget for planning this item; past it the item fails and nothing is written."}),
                    ),
                    (
                        "dryRun",
                        json!({"type": "boolean", "default": false, "description": "Return the unified diffs and counts without writing. dryRun on any item previews the whole call, so nothing is written."}),
                    ),
                ]),
                vec!["file_path"],
            )),
        )]),
        vec!["items"],
    )
}
fn file_diff_schema() -> Value {
    object_schema(
        prop(vec![(
            "items",
            array_of(item_object(
                prop(vec![
                    (
                        "path",
                        json!({"type": "string", "description": "Old (left) file."}),
                    ),
                    (
                        "otherPath",
                        json!({"type": "string", "description": "New (right) file. Give otherPath or content/content_path."}),
                    ),
                    (
                        "content",
                        json!({"type": "string", "description": "Inline new (right) text compared against path."}),
                    ),
                    ("content_path", string()),
                    ("content_offset", number_default(0)),
                    ("content_length", number()),
                    (
                        "contextLines",
                        json!({"type": "integer", "minimum": 0, "maximum": 10000, "default": 3, "description": "Unchanged lines around each hunk; default 3."}),
                    ),
                    ("ignoreWhitespace", boolean_default(false)),
                    (
                        "algorithm",
                        enum_str(&["myers", "patience", "histogram"], Some("myers")),
                    ),
                ]),
                vec!["path"],
            )),
        )]),
        vec!["items"],
    )
}
fn path_find_schema() -> Value {
    object_schema(
        prop(vec![(
            "items",
            array_of(item_object(
                prop(vec![
                    (
                        "path",
                        json!({"type": "string", "description": "Root directory to search."}),
                    ),
                    (
                        "pattern",
                        json!({"type": "string", "description": "Regex matched against the entry name (or the root-relative path with fullPath). Omit to list everything that passes the filters."}),
                    ),
                    (
                        "glob",
                        json!({"type": "string", "description": "Glob matched against the entry name (or the root-relative path with fullPath), e.g. *.rs; '|' separates alternatives. With fullPath, * stops at / so use **/*.rs for any depth."}),
                    ),
                    (
                        "fullPath",
                        json!({"type": "boolean", "default": false, "description": "Match pattern/glob against the root-relative path with / separators instead of the name."}),
                    ),
                    (
                        "ignoreCase",
                        json!({"type": "boolean", "description": "Omit for smart-case (case-insensitive unless the pattern has an uppercase letter); true or false forces it."}),
                    ),
                    (
                        "type",
                        enum_str(&["any", "file", "directory", "symlink"], Some("any")),
                    ),
                    (
                        "extension",
                        json!({"type": "string", "description": "File extension filter without the dot, e.g. rs or rs|toml."}),
                    ),
                    (
                        "size",
                        json!({"type": "string", "description": "Size filter like fd -S: +10k (at least), -1M (at most), 500b (exactly); a unit is required; units b, k, m, g, t (1000-based) and ki, mi, gi, ti (1024-based); comma joins bounds; files only."}),
                    ),
                    (
                        "changedWithin",
                        json!({"type": "string", "description": "Modified within a duration (30m, 2h, 7d, 2w) or since a date (2026-11-01, 2026-11-01 09:00:00, RFC 3339, or @epoch seconds); dates without an offset are local time like fd (a DST overlap takes the later instant, a gap the pre-transition offset; an IANA name in TZ needs ICU on Windows 10 1703+)."}),
                    ),
                    (
                        "changedBefore",
                        json!({"type": "string", "description": "Modified before a duration ago or a date; same syntax as changedWithin."}),
                    ),
                    (
                        "empty",
                        json!({"type": "boolean", "default": false, "description": "Only empty files and empty directories."}),
                    ),
                    ("minDepth", integer_min(0)),
                    (
                        "maxDepth",
                        json!({"type": "integer", "minimum": 0, "description": "Deepest level to visit; 1 lists direct children only."}),
                    ),
                    ("includeHidden", boolean_default(false)),
                    (
                        "respectGitignore",
                        json!({"type": "boolean", "default": true, "description": "Skip entries ignored by repository .gitignore files."}),
                    ),
                    (
                        "noDefaultExcludes",
                        json!({"type": "boolean", "default": false, "description": "Set true to also walk node_modules/, target/, and .git/ (skipped by default)."}),
                    ),
                    (
                        "excludePatterns",
                        json!({"type": "array", "items": {"type": "string"}, "default": [], "description": "Globs to exclude, matched like fd --exclude."}),
                    ),
                    ("followLinks", boolean_default(false)),
                    (
                        "sortBy",
                        json!({"type": "string", "enum": ["path", "modified", "size"], "default": "path", "description": "path (ascending, component-wise: a < a/b < a-x), modified (newest first), or size (largest first). maxResults keeps the exact first entries in this order."}),
                    ),
                    (
                        "details",
                        json!({"type": "boolean", "default": false, "description": "Append a tab, the size in bytes, a tab, and the UTC RFC 3339 modification time to each entry; directories omit the size."}),
                    ),
                    (
                        "maxResults",
                        json!({"type": "integer", "minimum": 1, "description": "Stop after this many entries (limitReached:true when more exist)."}),
                    ),
                    ("timeout_ms", number_default(25000)),
                ]),
                vec!["path"],
            )),
        )]),
        vec!["items"],
    )
}
fn ast_search_schema() -> Value {
    object_schema(
        prop(vec![(
            "items",
            array_of(item_object(
                prop(vec![
                    (
                        "path",
                        json!({"type": "string", "description": "File or directory to search."}),
                    ),
                    (
                        "pattern",
                        json!({"type": "string", "description": "ast-grep pattern, e.g. $A.unwrap() or console.log($$$ARGS)."}),
                    ),
                    (
                        "lang",
                        json!({"type": "string", "description": "ast-grep language (rust, typescript, tsx, javascript, python, java, go, ...). Required when the pattern is ambiguous across a mixed tree."}),
                    ),
                    (
                        "rewrite",
                        json!({"type": "string", "description": "Preview this ast-grep rewrite as a unified diff per file; nothing is written."}),
                    ),
                    (
                        "globs",
                        json!({"type": "array", "items": {"type": "string"}, "description": "Include or !exclude globs (ast-grep --globs)."}),
                    ),
                    (
                        "strictness",
                        enum_str(&["cst", "smart", "ast", "relaxed", "signature"], None),
                    ),
                    (
                        "contextLines",
                        json!({"type": "integer", "minimum": 0, "maximum": 10000, "default": 0, "description": "Source lines shown around each match."}),
                    ),
                    ("includeHidden", boolean_default(false)),
                    ("respectGitignore", boolean_default(true)),
                    (
                        "maxResults",
                        json!({"type": "integer", "minimum": 1, "description": "Stop after this many matches (limitReached:true when more exist)."}),
                    ),
                    ("timeout_ms", number_default(25000)),
                ]),
                vec!["path", "pattern"],
            )),
        )]),
        vec!["items"],
    )
}
fn git_add_schema() -> Value {
    object_schema(
        prop(vec![
            ("path", string()),
            ("paths", string_array()),
            ("all", boolean()),
            ("update", boolean()),
            ("force", boolean()),
        ]),
        vec!["path"],
    )
}
fn git_amend_schema() -> Value {
    object_schema(
        prop(vec![
            ("path", string()),
            ("message", string()),
            ("messagePath", string()),
            ("messageOffset", number_default(0)),
            ("messageLength", number()),
            (
                "author",
                item_object(
                    prop(vec![
                        ("name", json!({"type": "string", "minLength": 1})),
                        ("email", json!({"type": "string", "format": "email"})),
                    ]),
                    vec!["name", "email"],
                ),
            ),
            ("resetAuthor", boolean()),
            ("allowEmpty", boolean()),
            ("noVerify", boolean()),
            ("filesToStage", string_array()),
        ]),
        vec!["path"],
    )
}
fn git_commit_schema() -> Value {
    object_schema(
        prop(vec![
            ("path", string()),
            ("message", string()),
            ("messagePath", string()),
            ("messageOffset", number_default(0)),
            ("messageLength", number()),
            (
                "author",
                item_object(
                    prop(vec![
                        ("name", json!({"type": "string", "minLength": 1})),
                        ("email", json!({"type": "string", "format": "email"})),
                    ]),
                    vec!["name", "email"],
                ),
            ),
            ("amend", boolean()),
            ("allowEmpty", boolean()),
            ("noVerify", boolean()),
            ("filesToStage", string_array()),
        ]),
        vec!["path"],
    )
}
fn git_diff_schema() -> Value {
    object_schema(
        prop(vec![
            ("path", string()),
            ("target", string()),
            ("source", string()),
            ("paths", string_array()),
            ("staged", boolean()),
            ("nameOnly", boolean()),
            ("stat", boolean()),
            ("check", boolean()),
            ("contextLines", integer_min(0)),
        ]),
        vec!["path"],
    )
}
fn git_show_schema() -> Value {
    object_schema(
        prop(vec![
            ("path", string()),
            ("object", string()),
            ("objects", string_array_min()),
            ("filePath", string()),
            ("format", json!({"type": "string", "enum": ["raw"]})),
            ("stat", boolean()),
        ]),
        vec!["path"],
    )
}
fn git_status_schema() -> Value {
    object_schema(
        prop(vec![("path", string()), ("includeUntracked", boolean())]),
        vec!["path"],
    )
}
fn web_dump_values() -> [&'static str; 5] {
    ["html", "text", "markdown", "links", "readability"]
}
fn web_fetch_schema() -> Value {
    let offset = json!({"type": "integer", "minimum": 0, "description": "Character offset into the rendered text."});
    let max_chars = json!({"type": "integer", "minimum": 1, "description": "Return at most this many rendered characters from offset."});
    object_schema(
        prop(vec![
            ("url", string()),
            (
                "items",
                array_of(item_object(
                    prop(vec![
                        ("url", string()),
                        ("dump", enum_str(&web_dump_values(), None)),
                        ("timeoutMs", number()),
                        ("maxBytes", number()),
                        ("userAgent", string()),
                        ("offset", offset.clone()),
                        ("maxChars", max_chars.clone()),
                    ]),
                    vec!["url"],
                )),
            ),
            (
                "dump",
                json!({"type": "string", "enum": web_dump_values(), "default": "markdown", "description": "Output format; default markdown."}),
            ),
            ("timeoutMs", number_default(20000)),
            ("maxBytes", number_default(20000000)),
            ("maxRedirects", number_default(5)),
            ("userAgent", string()),
            ("offset", offset),
            ("maxChars", max_chars),
        ]),
        vec![],
    )
}
fn web_extract_schema() -> Value {
    object_schema(
        prop(vec![(
            "items",
            array_of(item_object(
                prop(vec![
                    ("html", string()),
                    ("path", string()),
                    (
                        "dump",
                        json!({"type": "string", "enum": ["text", "markdown", "links", "readability"], "default": "markdown", "description": "Output format; default markdown."}),
                    ),
                    ("baseUrl", string()),
                ]),
                vec![],
            )),
        )]),
        vec!["items"],
    )
}
fn download_schema() -> Value {
    object_schema(
        prop(vec![
            (
                "items",
                array_of(item_object(
                    prop(vec![
                        ("url", string()),
                        ("path", string()),
                        ("maxBytes", number()),
                        ("timeoutMs", number()),
                        ("overwrite", boolean_default(false)),
                    ]),
                    vec!["url", "path"],
                )),
            ),
            ("maxBytes", number()),
            ("timeoutMs", number()),
            ("maxRedirects", number_default(5)),
            ("userAgent", string()),
        ]),
        vec!["items"],
    )
}
fn mysql_query_schema() -> Value {
    object_schema(
        prop(vec![
            (
                "sql",
                json!({"type": "string", "description": "One read-only statement: SELECT, SHOW, DESCRIBE, EXPLAIN, or WITH ... SELECT."}),
            ),
            (
                "sql_path",
                json!({"type": "string", "description": "Read the statement from this UTF-8 file instead of sql."}),
            ),
            (
                "maxRows",
                json!({"type": "integer", "minimum": 1, "description": "Row cap for this call, never above mysql.max_rows in config.toml (default 1000). A cut result reports truncated:true."}),
            ),
            (
                "timeout_ms",
                json!({"type": "integer", "minimum": 1, "description": "Time limit for this call, never above mysql.query_timeout_ms in config.toml (default 10000)."}),
            ),
        ]),
        vec![],
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_mysql_query_only_when_configured() {
        let names = |with_mysql: bool| {
            build_tool_catalog(with_mysql)
                .iter()
                .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(false).len(), 25);
        assert!(!names(false).contains(&"mysql-query".to_string()));
        assert_eq!(names(true).len(), 26);
        let catalog = build_tool_catalog(true);
        let mysql = catalog
            .iter()
            .find(|tool| tool["name"] == "mysql-query")
            .unwrap();
        assert_eq!(mysql["annotations"]["readOnlyHint"], true);
        assert!(mysql["inputSchema"]["properties"]["sql"].is_object());
        assert!(mysql["_meta"]["anthropic/alwaysLoad"].is_null());
    }
    #[test]
    fn exposes_expected_tool_surface() {
        let tools = tool_catalog();
        let mut names = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        names.sort();
        let mut expected = vec![
            "ast-search",
            "dir-create",
            "dir-list",
            "file-diff",
            "file-edit",
            "file-edit-lines",
            "file-read",
            "file-read-line-range",
            "file-replace",
            "file-write",
            "git-add",
            "git-amend",
            "git-commit",
            "git-diff",
            "git-show",
            "git-status",
            "path-copy",
            "path-find",
            "path-move",
            "path-remove",
            "path-stat",
            "fs-search",
            "web-fetch",
            "web-extract",
            "download-to-file",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(names, expected);
        assert!(tools.iter().all(|tool| {
            serde_json::to_string(&tool["inputSchema"])
                .unwrap()
                .contains("args_path")
        }));
    }
    #[test]
    fn marks_always_load_tools() {
        let tools = tool_catalog();
        let marked = tools
            .iter()
            .filter(|tool| tool["_meta"]["anthropic/alwaysLoad"] == json!(true))
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            marked,
            vec![
                "file-read",
                "file-read-line-range",
                "dir-list",
                "fs-search",
                "path-find",
                "path-stat",
                "file-edit",
                "file-edit-lines",
                "git-diff",
                "git-show",
                "git-status"
            ]
        );
    }
    #[test]
    fn declares_result_size_from_output_budget() {
        let expected = host_output_bytes(ClientHost::ClaudeCode).min(MAX_RESULT_SIZE_CHARS);
        assert!(
            tool_catalog()
                .iter()
                .all(|tool| tool["_meta"]["anthropic/maxResultSizeChars"] == json!(expected))
        );
    }
    #[test]
    fn exposes_precise_line_range_schema() {
        let tools = tool_catalog();
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == "file-read-line-range")
            .unwrap();
        let item_props = &tool["inputSchema"]["properties"]["items"]["items"]["properties"];

        assert!(item_props.get("start_line").is_some());
        assert!(item_props.get("line_count").is_some());
        assert!(item_props.get("offset").is_none());
        assert!(item_props.get("length").is_none());
        assert!(item_props.get("isUrl").is_none());
    }
    // edit-lines must mirror the line-range tool: line positions are 1-based positive
    // integers, not a loose number() that lets fractional / zero values reach the handler.
    #[test]
    fn edit_lines_uses_strict_line_integers() {
        let tools = tool_catalog();
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == "file-edit-lines")
            .unwrap();
        let item_props = &tool["inputSchema"]["properties"]["items"]["items"]["properties"];

        for key in ["start_line", "end_line"] {
            assert_eq!(item_props[key]["type"], json!("integer"), "{key} type");
            assert_eq!(item_props[key]["minimum"], json!(1), "{key} minimum");
        }
    }
    // git-diff advertised autoExclude but no handler ever honored it; contextLines is now
    // wired to --unified, so it stays.
    #[test]
    fn git_diff_drops_dead_auto_exclude() {
        let tools = tool_catalog();
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == "git-diff")
            .unwrap();
        let props = &tool["inputSchema"]["properties"];

        assert!(props.get("autoExclude").is_none());
        assert!(props.get("includeUntracked").is_none());
        assert!(props.get("contextLines").is_some());
    }
    // file-read advertised an unused generic `options` bag; no handler ever read it.
    #[test]
    fn file_read_drops_dead_options() {
        let tools = tool_catalog();
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == "file-read")
            .unwrap();
        let item_props = &tool["inputSchema"]["properties"]["items"]["items"]["properties"];

        assert!(item_props.get("options").is_none());
        assert!(item_props.get("path").is_some());
    }
    // 호스트 표시 제목, 힌트 기본값 명시
    #[test]
    fn declares_titles_and_explicit_hints() {
        let tools = tool_catalog();
        let open_world = ["file-read", "web-fetch", "download-to-file"];
        for tool in &tools {
            let name = tool["name"].as_str().unwrap();
            let annotations = &tool["annotations"];
            assert_eq!(tool["title"], annotations["title"], "{name}");
            assert_ne!(tool["title"], json!(name), "{name}");
            assert_eq!(
                annotations["openWorldHint"],
                json!(open_world.contains(&name)),
                "{name}"
            );
            if annotations["readOnlyHint"] == json!(false) {
                assert!(annotations["destructiveHint"].is_boolean(), "{name}");
            }
            let schema = serde_json::to_string(&tool["inputSchema"]).unwrap();
            assert!(!schema.contains("exclusiveMinimum"), "{name}");
        }
        let git_add = tools.iter().find(|tool| tool["name"] == "git-add").unwrap();
        assert_eq!(git_add["annotations"]["destructiveHint"], json!(true));
    }
    // git-diff check flag drives `git diff --check`; it must be advertised in the schema.
    #[test]
    fn git_diff_advertises_check() {
        let tools = tool_catalog();
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == "git-diff")
            .unwrap();
        assert!(tool["inputSchema"]["properties"].get("check").is_some());
    }
    // 미리보기 기본 꺼짐, 검색 도구 읽기 전용
    #[test]
    fn integrated_tools_declare_preview_and_hints() {
        let tools = tool_catalog();
        let find = |name: &str| tools.iter().find(|tool| tool["name"] == name).unwrap();
        for name in ["file-edit", "file-edit-lines", "file-replace"] {
            let item_props =
                &find(name)["inputSchema"]["properties"]["items"]["items"]["properties"];
            assert_eq!(item_props["dryRun"]["default"], json!(false), "{name}");
        }
        for name in ["path-find", "ast-search", "file-diff"] {
            assert_eq!(
                find(name)["annotations"]["readOnlyHint"],
                json!(true),
                "{name}"
            );
        }
        let replace = &find("file-replace")["annotations"];
        assert_eq!(replace["readOnlyHint"], json!(false));
        assert_eq!(replace["destructiveHint"], json!(true));
    }
}
