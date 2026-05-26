use serde_json::{Map, Value, json};
use std::env;

const CMD_PRF_DSC: &str = "For large arguments, pass a UTF-8 JSON file via {\"args_path\":\"ABSOLUTE_PATH_TO_ARGS_JSON\"}.";
const BTCH_GDNC: &str = "Batch same-kind operations into one call.";
const PTH_GDNC: &str =
    "Use absolute paths. Relative paths depend on the current working directory.";

// 1. Tool catalog ―――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――――
pub fn tool_catalog() -> Vec<Value> {
    let profile = env::var("RUST_FS_MCP_TOOL_PROFILE").unwrap_or_else(|_| "full".to_string());
    tool_catalog_for_profile(&profile)
}

pub fn tool_catalog_for_profile(profile: &str) -> Vec<Value> {
    let tools = full_tool_catalog();
    if profile == "fast-coding" {
        return tools
            .into_iter()
            .filter(|tool| tool.get("name").and_then(Value::as_str) == Some("fs-inspect"))
            .collect();
    }

    tools
}

fn full_tool_catalog() -> Vec<Value> {
    vec![
        tool(
            "file-read",
            "Read Files",
            &format!(
                "Read files in parallel.\nUse paths for simple reads or items for offset, length, headers, or URL reads.\nSet allowMissing true to return missing local paths as non-error missing results.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            read_schema(),
            true,
            None,
            Some(true),
        ),
        tool(
            "file-lines",
            "Read Files With Line Numbers",
            &format!(
                "Read text files in parallel with 1-based line numbers.\nUse paths for simple reads or items for offset and length.\nSet allowMissing true to return missing local paths as non-error missing results.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            read_schema(),
            true,
            None,
            Some(true),
        ),
        tool(
            "file-write",
            "Write Files",
            &format!(
                "Write files in parallel.\nPrefer content_path or args_path for large text.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            write_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "dir-mk",
            "Create Directories",
            &format!(
                "Create one or many directories in parallel.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            dir_mk_schema(),
            false,
            Some(false),
            None,
        ),
        tool(
            "dir-list",
            "List Directories",
            &format!(
                "List one or many directories in parallel.\nUse items: [{{ path, depth?, maxEntries?, excludePatterns?, includeFiles? }}].\nSet allowMissing true to return missing local paths as non-error missing results.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            dir_list_schema(),
            true,
            None,
            None,
        ),
        tool(
            "file-copy",
            "Copy Files",
            &format!(
                "Copy one or many files or directories in parallel.\nUse items: [{{ source, destination, recursive?, force? }}].\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            copy_schema(),
            false,
            Some(false),
            Some(false),
        ),
        tool(
            "file-move",
            "Move/Rename Files",
            &format!(
                "Move or rename one or many files in parallel.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            move_schema(),
            false,
            Some(true),
            Some(false),
        ),
        tool(
            "file-remove",
            "Remove Files",
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
            "Start Searches",
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
            "Regex Searches",
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
            "Get Full Search Results",
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
            "Stop Searches",
            &format!("Stop one or many active searches in parallel.\n{BTCH_GDNC}\n{CMD_PRF_DSC}"),
            search_stop_schema(),
            false,
            Some(false),
            None,
        ),
        tool(
            "file-infos",
            "Get File Information",
            &format!(
                "Retrieve metadata for one or many files in parallel.\nSet allowMissing true to return missing local paths as non-error missing results.\n{BTCH_GDNC}\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            infos_schema(),
            true,
            None,
            None,
        ),
        tool(
            "file-edit",
            "Edit Blocks",
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
            "Edit Line Ranges",
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
            "Git Add",
            &format!("Stage files for commit.\n{CMD_PRF_DSC}"),
            git_add_schema(),
            false,
            None,
            None,
        ),
        tool(
            "git-commit",
            "Git Commit",
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
            "Git Diff",
            &format!(
                "Show differences between commits, branches, or working tree state.\n{CMD_PRF_DSC}"
            ),
            git_diff_schema(),
            true,
            None,
            None,
        ),
        tool(
            "git-cwd",
            "Git Set Working Directory",
            &format!(
                "Pin the session git working directory and return a repository snapshot.\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            git_cwd_schema(),
            false,
            Some(true),
            None,
        ),
        tool(
            "git-show",
            "Git Show",
            &format!("Show a git object or file content at a revision.\n{CMD_PRF_DSC}"),
            git_show_schema(),
            true,
            None,
            None,
        ),
        tool(
            "git-status",
            "Git Status",
            &format!("Show working tree status, staging, and conflicts.\n{CMD_PRF_DSC}"),
            git_status_schema(),
            true,
            None,
            None,
        ),
        tool(
            "fs-inspect",
            "FS Inspect",
            &format!(
                "Run compact read-only filesystem inspection requests in one call for coding tasks. Supports count-files, search, json-pick, and snippet operations with short source snippets. For count-files, use glob or pattern for filename matching.\n{PTH_GDNC}\n{CMD_PRF_DSC}"
            ),
            inspect_schema(),
            true,
            None,
            None,
        ),
    ]
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
            (
                "options",
                json!({"type": "object", "additionalProperties": {}}),
            ),
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

fn dir_mk_schema() -> Value {
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
            ("earlyTermination", boolean()),
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
                json!({"type": "string", "enum": ["count-files", "search", "json-pick", "snippet"]}),
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
                    ("start_line", number()),
                    ("end_line", number()),
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
            ("includeUntracked", boolean()),
            ("nameOnly", boolean()),
            ("stat", boolean()),
            ("contextLines", integer_min(0)),
            ("autoExclude", boolean()),
        ]),
        vec![],
    )
}

fn git_cwd_schema() -> Value {
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
            ("filePath", string()),
            ("format", json!({"type": "string", "enum": ["raw"]})),
            ("stat", boolean()),
        ]),
        vec!["object"],
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
            "dir-list",
            "dir-mk",
            "file-copy",
            "file-edit",
            "file-edit-lines",
            "file-infos",
            "file-lines",
            "file-move",
            "file-read",
            "file-remove",
            "file-write",
            "git-add",
            "git-commit",
            "git-cwd",
            "git-diff",
            "git-show",
            "git-status",
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
}
