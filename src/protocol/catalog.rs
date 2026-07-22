//! catalog.rs
//! protocol::catalog
//!
//! Single source of truth for the public tool catalog (name, description, annotation, JSON input schema) exposed by tools/list.
//! Caches the fixed full catalog in OnceLock.
//! Marks the fixed always-load tool set with _meta {"anthropic/alwaysLoad": true} so
//! schema-deferring hosts expose those tools upfront.
//!

use serde_json::{json, Map, Value};
use std::sync::OnceLock;

const CMD_PRF_DSC: &str = "For large arguments, pass a UTF-8 JSON file via {\"args_path\":\"ABSOLUTE_PATH_TO_ARGS_JSON\"}.";
const BTCH_GDNC: &str = "Batch same-kind operations into one call.";
const PTH_GDNC: &str = "Use absolute paths. Relative paths depend on the current working directory.";
static FULL_TOOL_CATALOG: OnceLock<Vec<Value>> = OnceLock::new();
static WIRE_TOOLS_JSON: OnceLock<String> = OnceLock::new();
const ALWAYS_LOAD: &[&str] = &["file-read", "fs-search", "file-edit-lines", "file-edit", "git-status", "git-diff", "dir-list", "file-read-line-range", "path-stat", "git-show"];

// 1. Tool catalog ---------------------------------------------------------------------------
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
  FULL_TOOL_CATALOG.get_or_init(build_full_tool_catalog)
}
fn build_full_tool_catalog() -> Vec<Value> {
  let mut tools = vec![
    tool("file-read", "file-read", &format!("Read files in parallel.\nWhen a task needs 2+ files, put them all in one paths[] (or items) call instead of calling file-read once per file.\nUse paths for simple reads or items for offset, length, headers, or URL reads.\nSet allowMissing true to return missing local paths as non-error missing results.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), read_schema(), true, None, Some(true)),
    tool("file-read-line-range", "file-read-line-range", &format!("Read ranges from local text files and return each line with its 1-based line number.\nWhen a task needs ranges from 2+ files, put them all in one items call instead of one call per file.\nUse paths to read complete files or items with start_line and line_count for bounded ranges.\nSet allowMissing true to return missing local paths as non-error missing results.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), line_range_schema(), true, None, Some(false)),
    tool("file-write", "file-write", &format!("Write files in parallel.\nPrefer content_path or args_path for large text.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), write_schema(), false, Some(true), Some(false)),
    tool("dir-create", "dir-create", &format!("Create one or many directories in parallel.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), dir_create_schema(), false, Some(false), None),
    tool("dir-list", "dir-list", &format!("List one or many directories in parallel.\nUse items: [{{ path, depth?, maxEntries?, excludePatterns?, includeFiles? }}].\nSet allowMissing true to return missing local paths as non-error missing results.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), dir_list_schema(), true, None, None),
    tool("path-copy", "path-copy", &format!("Copy one or many files or directories in parallel.\nUse items: [{{ source, destination, recursive?, force? }}].\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), copy_schema(), false, Some(false), Some(false)),
    tool("path-move", "path-move", &format!("Move or rename one or many files in parallel.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), move_schema(), false, Some(true), Some(false)),
    tool("path-remove", "path-remove", &format!("Delete one or many files or directories in parallel.\nUse paths: [\"...\"] for simple deletes (optionally with shared recursive/force), or items: [{{ path, recursive?, force? }}] for per-path flags.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), remove_schema(), false, Some(true), Some(false)),
    tool("fs-search", "fs-search", &format!("Run ripgrep-compatible regular-expression content searches directly.\nPrefer this over shell rg when regex search is needed.\nPattern flavor is Rust regex: linear time, Unicode-aware \\d \\w \\b (Hangul word boundaries work); look-around and backreferences are rejected with a rewrite hint, and plain regex syntax errors fall back to a literal search with the parse-error gist in the backend label.\nliteral:true forces fixed-string search (rg -F), wordMatch:true adds word boundaries (rg -w), multiline:true lets patterns span lines with . matching newlines (rg -U); per-item timeout_ms defaults to 25000.\nSkips **/node_modules/** and **/target/** by default (and .git with includeHidden) unless the search root is inside one; disable with noDefaultExcludes:true.\npattern_path can reduce transport overhead, and filePattern can narrow the target set.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), search_regex_schema(), true, None, None),
    tool("path-stat", "path-stat", &format!("Retrieve metadata for one or many filesystem paths in parallel.\nSet allowMissing true to return missing local paths as non-error missing results.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), infos_schema(), true, None, None),
    tool("file-edit", "file-edit", &format!("Apply exact block replacements in parallel.\nPrefer *_path or args_path for large text.\nFor large or multi-file writes/edits, prefer fs-mcp batch tools with *_path or args_path.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), edit_schema(), false, Some(true), Some(false)),
    tool("file-edit-lines", "file-edit-lines", &format!("Replace, insert, or delete by 1-based line numbers. PREFER over file-edit when line numbers are known (faster, no EOL crafting). EOL auto-detected from file. Use `after: true` to insert after end_line without removing it.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), edit_lines_schema(), false, Some(true), Some(false)),
    tool("git-add", "git-add", &format!("Stage files for a repository path.\nPass multiple files in one paths[] call.\n{CMD_PRF_DSC}"), git_add_schema(), false, None, None),
    tool("git-amend", "git-amend", &format!("Amend the last commit in a repository path.\nOmit message to keep it (--no-edit); pass a Conventional Commit message to rewrite it.\nUse filesToStage to add changes and resetAuthor to reset authorship.\n{CMD_PRF_DSC}"), git_amend_schema(), false, Some(true), None),
    tool("git-commit", "git-commit", &format!("Create a commit in a repository path.\nUse a multi-line Conventional Commit message (lowercase English type; the summary may be any language).\n<type>: <summary>\n- <change detail>\n- <verification or behavior detail>\nUse messagePath for long messages.\n{CMD_PRF_DSC}"), git_commit_schema(), false, Some(true), None),
    tool("git-diff", "git-diff", &format!("Show differences for a repository path.\nUse paths[] to scope the diff to specific files in one call, nameOnly for a changed-file list, and check to flag whitespace errors and leftover conflict markers.\n{CMD_PRF_DSC}"), git_diff_schema(), true, None, None),
    tool("git-show", "git-show", &format!("Show git objects or file content for a repository path.\nUse objects[] to fetch several revisions in one call, and stat true for a diffstat instead of the full patch.\n{BTCH_GDNC}\n{CMD_PRF_DSC}"), git_show_schema(), true, None, None),
    tool("git-status", "git-status", &format!("Show working tree status, staging, and conflicts for a repository path.\n{CMD_PRF_DSC}"), git_status_schema(), true, None, None),
    tool("fs-inspect", "fs-inspect", &format!("Run compact read-only filesystem inspection requests in one call for coding tasks. Supports count-files, search, json-pick, snippet, and git-status operations. Bundle file reads, content search, and a git-status/branch lookup into a SINGLE call to avoid multiple tool round-trips. For count-files, use glob or pattern for filename matching; git-status takes an optional path (defaults to root). Traversal stops at an internal ~25s time budget and returns partial results with warnings.\n{PTH_GDNC}\n{CMD_PRF_DSC}"), inspect_schema(), true, None, None),
    tool("web-fetch", "web-fetch", &format!("Fetch one or many URLs over HTTP/HTTPS (no browser) and return the body as markdown, text, links, readability main-content, or raw html.\nTIER-1 fast path: use for static or server-rendered pages and JSON/XHR endpoints; for client-rendered JS/SPA pages use web-render.\nBatch many URLs in one items[] call. Private/link-local addresses are blocked (SSRF guard); loopback (localhost) is allowed for local dev servers.\n{BTCH_GDNC}\n{CMD_PRF_DSC}"), web_fetch_schema(), true, None, Some(true)),
    tool("web-render", "web-render", &format!("Render one URL in the obscura headless browser (JS/SPA, waits, CSS selector, stealth) and dump html, text, or links.\nTIER-2 escalation for pages web-fetch cannot read (client-side rendering, interaction, JS anti-bot). Slower and heavier than web-fetch, so try web-fetch first.\n{CMD_PRF_DSC}"), web_render_schema(), true, None, Some(true)),
    tool("web-extract", "web-extract", &format!("Convert already-held HTML (inline html or a local file path) into markdown, plain text, links, or readability main-content. No network access.\nUse when you already have HTML and only need clean extraction. baseUrl resolves relative links.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), web_extract_schema(), true, None, Some(false)),
    tool("download-to-file", "download-to-file", &format!("Download one or many URLs to local files over HTTP/HTTPS (streamed to a temp file, then renamed - partial downloads never land on the destination).\nThe SSRF guard blocks private/link-local hosts; loopback (localhost) is allowed. Set overwrite:true to replace an existing file.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"), download_schema(), false, Some(true), Some(true)),
  ];
  for tool in tools.iter_mut() {
    let name = tool["name"].as_str().unwrap_or("");
    if ALWAYS_LOAD.contains(&name) {
      tool["_meta"] = json!({ "anthropic/alwaysLoad": true });
    }
  }
  tools
}
// 2. Tool entry ----------------------------------------------------------------------------
fn tool(name: &str, title: &str, description: &str, input_schema: Value, read_only: bool, destructive: Option<bool>, open_world: Option<bool>) -> Value {
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
    "description": description,
    "inputSchema": input_schema,
    "annotations": annotations
  })
}
// 3. Schema helpers ------------------------------------------------------------------------
fn object_schema(properties: Map<String, Value>, required: Vec<&str>) -> Value {
  let mut props = properties;
  props.insert("args_path".to_string(), json!({"type": "string", "description": "Path to a UTF-8 JSON file containing the complete arguments for this tool."}));
  props.insert("args_offset".to_string(), json!({"type": "number", "default": 0, "description": "Optional character offset inside args_path."}));
  props.insert("args_length".to_string(), json!({"type": "number", "description": "Optional character length to read from args_path."}));
  let mut schema = Map::new();
  schema.insert("type".to_string(), json!("object"));
  schema.insert("properties".to_string(), Value::Object(props));
  if !required.is_empty() {
    schema.insert("required".to_string(), json!(required));
  }
  schema.insert("additionalProperties".to_string(), json!(false));
  schema.insert("$schema".to_string(), json!("http://json-schema.org/draft-07/schema#"));
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
  entries.into_iter().map(|(key, value)| (key.to_string(), value)).collect()
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
fn allow_missing() -> Value {
  json!({
    "type":
      "boolean",
    "default": false,
    "description":
      "When true, missing local paths are returned as non-error missing results."
  })
}
// 4. Public schemas ------------------------------------------------------------------------
fn read_item_schema() -> Value {
  item_object(prop(vec![("path", string()), ("isUrl", boolean_default(false)), ("offset", number_default(0)), ("length", number())]), vec!["path"])
}
fn read_schema() -> Value {
  object_schema(prop(vec![("allowMissing", allow_missing()), ("paths", string_array_min()), ("items", array_of(read_item_schema()))]), vec![])
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
  object_schema(prop(vec![("allowMissing", allow_missing()), ("paths", string_array_min()), ("items", array_of(line_range_item_schema()))]), vec![])
}
fn write_item_schema() -> Value {
  item_object(prop(vec![("path", string()), ("content_path", json!({"type": "string", "description": "Read UTF-8 content from this file. Preferred for large generated or pasted text."})), ("content", json!({"type": "string", "description": "Inline text accepted. For very large generated or pasted payloads, content_path or args_path can still reduce transport overhead."})), ("content_offset", number_default(0)), ("content_length", number()), ("mode", json!({"type": "string", "enum": ["rewrite", "append"], "default": "rewrite"}))]), vec!["path"])
}
fn write_schema() -> Value {
  object_schema(prop(vec![("items", array_of(write_item_schema()))]), vec!["items"])
}
fn dir_create_schema() -> Value {
  object_schema(prop(vec![("paths", string_array_min())]), vec!["paths"])
}
fn dir_item_schema() -> Value {
  item_object(prop(vec![("path", string()), ("depth", number_default(2)), ("maxEntries", json!({"type": "integer", "exclusiveMinimum": 0})), ("excludePatterns", json!({"type": "array", "items": {"type": "string"}, "default": []})), ("includeFiles", boolean_default(true))]), vec!["path"])
}
fn dir_list_schema() -> Value {
  object_schema(prop(vec![("allowMissing", allow_missing()), ("items", array_of(dir_item_schema()))]), vec!["items"])
}
fn copy_schema() -> Value {
  object_schema(prop(vec![("items", array_of(item_object(prop(vec![("source", string()), ("destination", string()), ("recursive", boolean_default(false)), ("force", boolean_default(false))]), vec!["source", "destination"])))]), vec!["items"])
}
fn move_schema() -> Value {
  object_schema(prop(vec![("items", array_of(item_object(prop(vec![("source", string()), ("destination", string())]), vec!["source", "destination"])))]), vec!["items"])
}
fn remove_schema() -> Value {
  // paths[](간단 삭제) 또는 items[](항목별 플래그) 중 하나를 받는다(둘 다 선택, 핸들러가 존재 검증).
  object_schema(prop(vec![("paths", string_array_min()), ("recursive", boolean_default(false)), ("force", boolean_default(false)), ("items", array_of(item_object(prop(vec![("path", string()), ("recursive", boolean_default(false)), ("force", boolean_default(false))]), vec!["path"])))]), vec![])
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
      ("ignoreCase", boolean_default(true)),
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
      ("contextLines", number_default(2)),
      ("timeout_ms", number_default(25000)),
    ]),
    vec!["path"],
  )
}
fn search_regex_schema() -> Value {
  object_schema(prop(vec![("items", array_of(search_regex_item_schema()))]), vec!["items"])
}
fn evidence_extract_schema() -> Value {
  item_object(prop(vec![("name", string()), ("regex", string())]), vec!["name", "regex"])
}
fn inspect_request_schema() -> Value {
  item_object(prop(vec![("id", string()), ("op", json!({"type": "string", "enum": ["count-files", "search", "json-pick", "snippet", "git-status"]})), ("path", string()), ("glob", string()), ("recursive", boolean()), ("pattern", string()), ("literal", boolean_default(false)), ("filePattern", string()), ("maxMatches", number_default(20)), ("extract", array_of(evidence_extract_schema())), ("pointers", string_array()), ("patterns", string_array()), ("contextLines", number_default(2)), ("maxSnippets", number_default(10))]), vec!["op", "path"])
}
fn inspect_schema() -> Value {
  object_schema(prop(vec![("root", string()), ("requests", array_of(inspect_request_schema())), ("maxSnippetChars", number_default(6000)), ("mode", json!({"type": "string", "enum": ["strict", "balanced", "speed"], "default": "strict"}))]), vec!["root", "requests"])
}
fn infos_schema() -> Value {
  object_schema(prop(vec![("allowMissing", allow_missing()), ("paths", string_array_min())]), vec!["paths"])
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
        ]),
        vec!["file_path", "start_line"],
      )),
    )]),
    vec!["items"],
  )
}
fn edit_schema() -> Value {
  object_schema(prop(vec![("items", array_of(item_object(prop(vec![("file_path", string()), ("old_string", string()), ("old_string_path", string()), ("old_string_offset", number_default(0)), ("old_string_length", number()), ("new_string", string()), ("new_string_path", string()), ("new_string_offset", number_default(0)), ("new_string_length", number()), ("expected_replacements", number_default(1))]), vec!["file_path"])))]), vec!["items"])
}
fn git_add_schema() -> Value {
  object_schema(prop(vec![("path", string()), ("paths", string_array()), ("all", boolean()), ("update", boolean()), ("force", boolean())]), vec!["path"])
}
fn git_amend_schema() -> Value {
  object_schema(prop(vec![("path", string()), ("message", string()), ("messagePath", string()), ("messageOffset", number_default(0)), ("messageLength", number()), ("author", item_object(prop(vec![("name", json!({"type": "string", "minLength": 1})), ("email", json!({"type": "string", "format": "email"}))]), vec!["name", "email"])), ("resetAuthor", boolean()), ("allowEmpty", boolean()), ("noVerify", boolean()), ("filesToStage", string_array())]), vec!["path"])
}
fn git_commit_schema() -> Value {
  object_schema(prop(vec![("path", string()), ("message", string()), ("messagePath", string()), ("messageOffset", number_default(0)), ("messageLength", number()), ("author", item_object(prop(vec![("name", json!({"type": "string", "minLength": 1})), ("email", json!({"type": "string", "format": "email"}))]), vec!["name", "email"])), ("amend", boolean()), ("allowEmpty", boolean()), ("noVerify", boolean()), ("filesToStage", string_array())]), vec!["path"])
}
fn git_diff_schema() -> Value {
  object_schema(prop(vec![("path", string()), ("target", string()), ("source", string()), ("paths", string_array()), ("staged", boolean()), ("nameOnly", boolean()), ("stat", boolean()), ("check", boolean()), ("contextLines", integer_min(0))]), vec!["path"])
}
fn git_show_schema() -> Value {
  object_schema(prop(vec![("path", string()), ("object", string()), ("objects", string_array_min()), ("filePath", string()), ("format", json!({"type": "string", "enum": ["raw"]})), ("stat", boolean())]), vec!["path"])
}
fn git_status_schema() -> Value {
  object_schema(prop(vec![("path", string()), ("includeUntracked", boolean())]), vec!["path"])
}
fn web_dump_values() -> [&'static str; 5] {
  ["html", "text", "markdown", "links", "readability"]
}
fn web_fetch_schema() -> Value {
  object_schema(prop(vec![("url", string()), ("items", array_of(item_object(prop(vec![("url", string()), ("dump", enum_str(&web_dump_values(), None)), ("timeoutMs", number()), ("maxBytes", number()), ("userAgent", string())]), vec!["url"]))), ("dump", enum_str(&web_dump_values(), Some("markdown"))), ("timeoutMs", number_default(20000)), ("maxBytes", number_default(5000000)), ("maxRedirects", number_default(5)), ("userAgent", string())]), vec![])
}
fn web_render_schema() -> Value {
  // evalScript는 스키마에서 제외: 구현이 SSRF 우회 가능성 때문에 무조건 거부하므로
  // 모델에 노출하면 실패 왕복만 늘어난다(서버측 거부 가드는 유지).
  object_schema(prop(vec![("url", string()), ("dump", enum_str(&["html", "text", "links"], Some("html"))), ("selector", string()), ("wait", number_default(5)), ("timeout", number_default(120)), ("waitUntil", string()), ("userAgent", string()), ("stealth", boolean_default(false)), ("quiet", boolean_default(false))]), vec!["url"])
}
fn web_extract_schema() -> Value {
  object_schema(prop(vec![("items", array_of(item_object(prop(vec![("html", string()), ("path", string()), ("dump", enum_str(&["text", "markdown", "links", "readability"], Some("markdown"))), ("baseUrl", string())]), vec![])))]), vec!["items"])
}
fn download_schema() -> Value {
  object_schema(prop(vec![("items", array_of(item_object(prop(vec![("url", string()), ("path", string()), ("maxBytes", number()), ("timeoutMs", number()), ("overwrite", boolean_default(false))]), vec!["url", "path"]))), ("maxBytes", number()), ("timeoutMs", number()), ("maxRedirects", number_default(5)), ("userAgent", string())]), vec!["items"])
}
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn exposes_expected_tool_surface() {
    let tools = tool_catalog();
    let mut names = tools.iter().map(|tool| tool["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    names.sort();
    let mut expected = vec!["dir-create", "dir-list", "file-edit", "file-edit-lines", "file-read", "file-read-line-range", "file-write", "git-add", "git-amend", "git-commit", "git-diff", "git-show", "git-status", "path-copy", "path-move", "path-remove", "path-stat", "fs-search", "fs-inspect", "web-fetch", "web-render", "web-extract", "download-to-file"].into_iter().map(str::to_string).collect::<Vec<_>>();
    expected.sort();
    assert_eq!(names, expected);
    assert!(tools.iter().all(|tool| { serde_json::to_string(&tool["inputSchema"]).unwrap().contains("args_path") }));
  }
  #[test]
  fn marks_always_load_tools() {
    let tools = tool_catalog();
    let marked = tools.iter().filter(|tool| tool["_meta"]["anthropic/alwaysLoad"] == json!(true)).map(|tool| tool["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(marked, vec!["file-read", "file-read-line-range", "dir-list", "fs-search", "path-stat", "file-edit", "file-edit-lines", "git-diff", "git-show", "git-status"]);
  }
  #[test]
  fn exposes_precise_line_range_schema() {
    let tools = tool_catalog();
    let tool = tools.iter().find(|tool| tool["name"] == "file-read-line-range").unwrap();
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
    let tool = tools.iter().find(|tool| tool["name"] == "file-edit-lines").unwrap();
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
    let tool = tools.iter().find(|tool| tool["name"] == "git-diff").unwrap();
    let props = &tool["inputSchema"]["properties"];

    assert!(props.get("autoExclude").is_none());
    assert!(props.get("includeUntracked").is_none());
    assert!(props.get("contextLines").is_some());
  }
  // file-read advertised an unused generic `options` bag; no handler ever read it.
  #[test]
  fn file_read_drops_dead_options() {
    let tools = tool_catalog();
    let tool = tools.iter().find(|tool| tool["name"] == "file-read").unwrap();
    let item_props = &tool["inputSchema"]["properties"]["items"]["items"]["properties"];

    assert!(item_props.get("options").is_none());
    assert!(item_props.get("path").is_some());
  }
  // git-diff check flag drives `git diff --check`; it must be advertised in the schema.
  #[test]
  fn git_diff_advertises_check() {
    let tools = tool_catalog();
    let tool = tools.iter().find(|tool| tool["name"] == "git-diff").unwrap();
    assert!(tool["inputSchema"]["properties"].get("check").is_some());
  }
}
