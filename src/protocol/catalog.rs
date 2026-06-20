//! catalog.rs
//! protocol::catalog
//!
//! Single source of truth for the public tool catalog (name, description, annotation, JSON input schema) exposed by tools/list.
//! Caches the full and fast-coding variants in OnceLock based on the RUST_FS_MCP_TOOL_PROFILE env var.
//! Marks RUST_FS_MCP_ALWAYS_LOAD tools (default file-read,search-regex,file-edit-lines) with _meta {"anthropic/alwaysLoad": true} so schema-deferring hosts expose them upfront.
//!

use serde_json::{Map, Value, json};
use std::env;
use std::sync::OnceLock;

const CMD_PRF_DSC: &str = "For large arguments, pass a UTF-8 JSON file via {\"args_path\":\"ABSOLUTE_PATH_TO_ARGS_JSON\"}.";
const BTCH_GDNC: &str = "Batch same-kind operations into one call.";
const PTH_GDNC: &str =
    "Use absolute paths. Relative paths depend on the current working directory.";
static FULL_TOOL_CATALOG: OnceLock<Vec<Value>> = OnceLock::new();
static FAST_CODING_TOOL_CATALOG: OnceLock<Vec<Value>> = OnceLock::new();
static ACTIVE_PROFILE: OnceLock<String> = OnceLock::new();

// 1. Tool catalog ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
// Removes the per-`tools/list` cost of an env::var call plus a full catalog clone via an OnceLock cache.
pub fn tool_catalog() -> Vec<Value> {
    let profile = ACTIVE_PROFILE.get_or_init(|| {
        env::var("RUST_FS_MCP_TOOL_PROFILE").unwrap_or_else(|_| "full".to_string())
    });
    tool_catalog_for_profile(profile)
}

pub fn tool_catalog_for_profile(profile: &str) -> Vec<Value> {
    if profile == "fast-coding" {
        return FAST_CODING_TOOL_CATALOG
            .get_or_init(|| {
                full_tool_catalog_ref()
                    .iter()
                    .filter(|tool| tool.get("name").and_then(Value::as_str) == Some("fs-inspect"))
                    .cloned()
                    .collect()
            })
            .clone();
    }
    full_tool_catalog_ref().clone()
}

fn full_tool_catalog_ref() -> &'static Vec<Value> {
    FULL_TOOL_CATALOG.get_or_init(build_full_tool_catalog)
}

fn build_full_tool_catalog() -> Vec<Value> {
    let mut tools = vec![
        tool(
            "file-read",
            "file-read",
            &format!(
                "Read files in parallel.\nUse paths for simple reads or items for offset, length, headers, or URL reads.\nSet allowMissing true to return missing local paths as non-error missing results.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            read_schema(),
            true,
            None,
            Some(true),
        ),
        tool(
            "file-read-line-range",
            "file-read-line-range",
            &format!(
                "Read ranges from local text files and return each line with its 1-based line number.\nUse paths to read complete files or items with start_line and line_count for bounded ranges.\nSet allowMissing true to return missing local paths as non-error missing results.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            line_range_schema(),
            true,
            None,
            Some(false),
        ),
        tool(
            "file-write",
            "file-write",
            &format!(
                "Write files in parallel.\nPrefer content_path or args_path for large text.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            write_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "dir-create",
            "dir-create",
            &format!(
                "Create one or many directories in parallel.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            dir_create_schema(),
            false,
            Some(false),
            None,
        ),
        tool(
            "dir-list",
            "dir-list",
            &format!(
                "List one or many directories in parallel.\nUse items: [{{ path, depth?, maxEntries?, excludePatterns?, includeFiles? }}].\nSet allowMissing true to return missing local paths as non-error missing results.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            dir_list_schema(),
            true,
            None,
            None,
        ),
        tool(
            "path-copy",
            "path-copy",
            &format!(
                "Copy one or many files or directories in parallel.\nUse items: [{{ source, destination, recursive?, force? }}].\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            copy_schema(),
            false,
            Some(false),
            Some(false),
        ),
        tool(
            "path-move",
            "path-move",
            &format!(
                "Move or rename one or many files in parallel.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            move_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "path-remove",
            "path-remove",
            &format!(
                "Delete one or many files or directories in parallel.\nUse items: [{{ path, recursive?, force? }}].\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            remove_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "search-start",
            "search-start",
            &format!(
                "Start searches in parallel.\npattern_path can reduce transport overhead, and filePattern can narrow the target set.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            search_start_schema(),
            true,
            None,
            None,
        ),
        tool(
            "search-regex",
            "search-regex",
            &format!(
                "Run ripgrep-compatible regular-expression content searches directly.\nPrefer this over shell rg when regex search is needed.\npattern_path can reduce transport overhead, and filePattern can narrow the target set.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            search_regex_schema(),
            true,
            None,
            None,
        ),
        tool(
            "search-get",
            "search-get",
            &format!(
                "Read one or many active search sessions in parallel with full per-item result text.\nUse offset or length for pagination.\n{BTCH_GDNC}\n{CMD_PRF_DSC}"
            ),
            search_get_schema(),
            true,
            None,
            None,
        ),
        tool(
            "search-stop",
            "search-stop",
            &format!("Stop one or many active searches in parallel.\n{BTCH_GDNC}\n{CMD_PRF_DSC}"),
            search_stop_schema(),
            false,
            Some(false),
            None,
        ),
        tool(
            "path-stat",
            "path-stat",
            &format!(
                "Retrieve metadata for one or many filesystem paths in parallel.\nSet allowMissing true to return missing local paths as non-error missing results.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            infos_schema(),
            true,
            None,
            None,
        ),
        tool(
            "file-edit",
            "file-edit",
            &format!(
                "Apply exact block replacements in parallel.\nPrefer *_path or args_path for large text.\nFor large or multi-file writes/edits, prefer fs-mcp batch tools with *_path or args_path.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            edit_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "file-edit-lines",
            "file-edit-lines",
            &format!(
                "Replace, insert, or delete by 1-based line numbers. PREFER over file-edit when line numbers are known (faster, no EOL crafting). EOL auto-detected from file. Use `after: true` to insert after end_line without removing it.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            edit_lines_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "git-add",
            "git-add",
            &format!("Stage files for commit.\n{CMD_PRF_DSC}"),
            git_add_schema(),
            false,
            None,
            None,
        ),
        tool(
            "git-amend",
            "git-amend",
            &format!(
                "Amend the last commit.\nOmit message to keep it (--no-edit); pass an English Conventional Commit message to rewrite it.\nUse filesToStage to add changes and resetAuthor to reset authorship.\n{CMD_PRF_DSC}"
            ),
            git_amend_schema(),
            false,
            Some(true),
            None,
        ),
        tool(
            "git-commit",
            "git-commit",
            &format!(
                "Create a commit from staged changes.\nUse an English multi-line Conventional Commit message.\n<type>: <summary>\n- <change detail>\n- <verification or behavior detail>\nUse messagePath for long messages.\n{CMD_PRF_DSC}"
            ),
            git_commit_schema(),
            false,
            Some(true),
            None,
        ),
        tool(
            "git-diff",
            "git-diff",
            &format!(
                "Show differences between commits, branches, or working tree state.\n{CMD_PRF_DSC}"
            ),
            git_diff_schema(),
            true,
            None,
            None,
        ),
        tool(
            "git-set-workdir",
            "git-set-workdir",
            &format!(
                "Set the session Git working directory and return a repository snapshot.\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            git_set_workdir_schema(),
            false,
            Some(true),
            None,
        ),
        tool(
            "git-show",
            "git-show",
            &format!(
                "Show git objects or file content at one or many revisions.\nUse objects[] to fetch several revisions in one call, and stat true for a diffstat instead of the full patch.\n{BTCH_GDNC}\n{CMD_PRF_DSC}"
            ),
            git_show_schema(),
            true,
            None,
            None,
        ),
        tool(
            "git-status",
            "git-status",
            &format!("Show working tree status, staging, and conflicts.\n{CMD_PRF_DSC}"),
            git_status_schema(),
            true,
            None,
            None,
        ),
        tool(
            "fs-inspect",
            "fs-inspect",
            &format!(
                "Run compact read-only filesystem inspection requests in one call for coding tasks. Supports count-files, search, json-pick, snippet, and git-status operations. Bundle file reads, content search, and a git-status/branch lookup into a SINGLE call to avoid multiple tool round-trips. For count-files, use glob or pattern for filename matching; git-status takes an optional path (defaults to root).\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            inspect_schema(),
            true,
            None,
            None,
        ),
    ];
    let always_load = env::var("RUST_FS_MCP_ALWAYS_LOAD")
        .unwrap_or_else(|_| "file-read,search-regex,file-edit-lines".to_string());
    for tool in tools.iter_mut() {
        let name = tool["name"].as_str().unwrap_or("");
        if !name.is_empty() && always_load.split(',').any(|entry| entry.trim() == name) {
            tool["_meta"] = json!({ "anthropic/alwaysLoad": true });
        }
    }
    tools
}

// 2. Tool entry ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
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
        "description": description,
        "inputSchema": input_schema,
        "annotations": annotations
    })
}

// 3. Schema helpers ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
fn object_schema(properties: Map<String, Value>, required: Vec<&str>) -> Value {
    let mut props = properties;
    props.insert(
        "args_path".to_string(),
        json!({"type": "string", "description": "Path to a UTF-8 JSON file containing the complete arguments for this tool."}),
    );
    props.insert(
        "args_offset".to_string(),
        json!({"type": "number", "default": 0, "description": "Optional character offset inside args_path."}),
    );
    props.insert(
        "args_length".to_string(),
        json!({"type": "number", "description": "Optional character length to read from args_path."}),
    );
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), Value::Object(props));
    if !required.is_empty() {
        schema.insert("required".to_string(), json!(required));
    }
    schema.insert("additionalProperties".to_string(), json!(false));
    schema.insert(
        "$schema".to_string(),
        json!("http://json-schema.org/draft-07/schema#"),
    );
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

fn allow_missing() -> Value {
    json!({
        "type": "boolean",
        "default": false,
        "description": "When true, missing local paths are returned as non-error missing results."
    })
}

// 4. Public schemas ――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
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
                    "type": "integer",
                    "minimum": 1,
                    "default": 1,
                    "description": "First line to return, using 1-based line numbers."
                }),
            ),
            (
                "line_count",
                json!({
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of lines to return. Omit to read through end of file."
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
                json!({"type": "string", "enum": ["rewrite", "append"], "default": "rewrite"}),
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
            ("depth", number_default(2)),
            (
                "maxEntries",
                json!({"type": "integer", "exclusiveMinimum": 0}),
            ),
            (
                "excludePatterns",
                json!({"type": "array", "items": {"type": "string"}, "default": []}),
            ),
            ("includeFiles", boolean_default(true)),
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
    object_schema(
        prop(vec![(
            "items",
            array_of(item_object(
                prop(vec![
                    ("path", string()),
                    ("recursive", boolean_default(false)),
                    ("force", boolean_default(false)),
                ]),
                vec!["path"],
            )),
        )]),
        vec!["items"],
    )
}

fn search_start_item_schema() -> Value {
    item_object(
        prop(vec![
            ("path", string()),
            ("pattern", string()),
            ("pattern_path", string()),
            ("pattern_offset", number_default(0)),
            ("pattern_length", number()),
            (
                "searchType",
                json!({"type": "string", "enum": ["files", "content"], "default": "files"}),
            ),
            ("filePattern", string()),
            ("ignoreCase", boolean_default(true)),
            ("maxResults", number()),
            ("includeHidden", boolean_default(false)),
            ("contextLines", number_default(5)),
            ("timeout_ms", number()),
            ("literalSearch", boolean_default(false)),
        ]),
        vec!["path"],
    )
}

fn search_start_schema() -> Value {
    object_schema(
        prop(vec![("items", array_of(search_start_item_schema()))]),
        vec!["items"],
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
            ("ignoreCase", boolean_default(true)),
            ("maxResults", number()),
            ("includeHidden", boolean_default(false)),
            ("contextLines", number_default(2)),
            ("timeout_ms", number_default(10000)),
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

fn search_get_schema() -> Value {
    object_schema(
        prop(vec![(
            "items",
            array_of(item_object(
                prop(vec![
                    ("sessionId", string()),
                    ("offset", number_default(0)),
                    ("length", number()),
                ]),
                vec!["sessionId"],
            )),
        )]),
        vec!["items"],
    )
}

fn search_stop_schema() -> Value {
    object_schema(
        prop(vec![("sessionIds", string_array_min())]),
        vec!["sessionIds"],
    )
}

fn evidence_extract_schema() -> Value {
    item_object(
        prop(vec![("name", string()), ("regex", string())]),
        vec!["name", "regex"],
    )
}

fn inspect_request_schema() -> Value {
    item_object(
        prop(vec![
            ("id", string()),
            (
                "op",
                json!({"type": "string", "enum": ["count-files", "search", "json-pick", "snippet", "git-status"]}),
            ),
            ("path", string()),
            ("glob", string()),
            ("recursive", boolean()),
            ("pattern", string()),
            ("literal", boolean_default(false)),
            ("filePattern", string()),
            ("maxMatches", number_default(20)),
            ("extract", array_of(evidence_extract_schema())),
            ("pointers", string_array()),
            ("patterns", string_array()),
            ("contextLines", number_default(2)),
            ("maxSnippets", number_default(10)),
        ]),
        vec!["op", "path"],
    )
}

fn inspect_schema() -> Value {
    object_schema(
        prop(vec![
            ("root", string()),
            ("requests", array_of(inspect_request_schema())),
            ("maxSnippetChars", number_default(6000)),
            (
                "mode",
                json!({"type": "string", "enum": ["strict", "balanced", "speed"], "default": "strict"}),
            ),
        ]),
        vec!["root", "requests"],
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
                    ("expected_lines", number()),
                ]),
                vec!["file_path", "start_line"],
            )),
        )]),
        vec!["items"],
    )
}

fn edit_schema() -> Value {
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
                    ("expected_replacements", number_default(1)),
                ]),
                vec!["file_path"],
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
        vec![],
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
        vec![],
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
        vec![],
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
            ("contextLines", integer_min(0)),
        ]),
        vec![],
    )
}

fn git_set_workdir_schema() -> Value {
    object_schema(
        prop(vec![
            ("path", string()),
            ("validateGitRepo", boolean()),
            ("initializeIfNotPresent", boolean()),
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
        vec![],
    )
}

fn git_status_schema() -> Value {
    object_schema(
        prop(vec![("path", string()), ("includeUntracked", boolean())]),
        vec![],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_expected_tool_surface() {
        let tools = tool_catalog();
        let mut names = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        names.sort();
        let mut expected = vec![
            "dir-create",
            "dir-list",
            "file-edit",
            "file-edit-lines",
            "file-read",
            "file-read-line-range",
            "file-write",
            "git-add",
            "git-amend",
            "git-commit",
            "git-diff",
            "git-show",
            "git-status",
            "git-set-workdir",
            "path-copy",
            "path-move",
            "path-remove",
            "path-stat",
            "search-get",
            "search-regex",
            "search-start",
            "search-stop",
            "fs-inspect",
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
        let tools = tool_catalog_for_profile("full");
        let marked = tools
            .iter()
            .filter(|tool| tool["_meta"]["anthropic/alwaysLoad"] == json!(true))
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(marked, vec!["file-read", "search-regex", "file-edit-lines"]);
    }

    #[test]
    fn exposes_precise_line_range_schema() {
        let tools = tool_catalog_for_profile("full");
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
        let tools = tool_catalog_for_profile("full");
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
        let tools = tool_catalog_for_profile("full");
        let tool = tools.iter().find(|tool| tool["name"] == "git-diff").unwrap();
        let props = &tool["inputSchema"]["properties"];

        assert!(props.get("autoExclude").is_none());
        assert!(props.get("includeUntracked").is_none());
        assert!(props.get("contextLines").is_some());
    }

    // search-start advertised earlyTermination but no handler ever read it.
    #[test]
    fn search_start_drops_dead_early_termination() {
        let tools = tool_catalog_for_profile("full");
        let tool = tools.iter().find(|tool| tool["name"] == "search-start").unwrap();
        let item_props = &tool["inputSchema"]["properties"]["items"]["items"]["properties"];

        assert!(item_props.get("earlyTermination").is_none());
    }

    // file-read advertised an unused generic `options` bag; no handler ever read it.
    #[test]
    fn file_read_drops_dead_options() {
        let tools = tool_catalog_for_profile("full");
        let tool = tools.iter().find(|tool| tool["name"] == "file-read").unwrap();
        let item_props = &tool["inputSchema"]["properties"]["items"]["items"]["properties"];

        assert!(item_props.get("options").is_none());
        assert!(item_props.get("path").is_some());
    }
}
