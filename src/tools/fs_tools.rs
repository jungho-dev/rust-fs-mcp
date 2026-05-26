use crate::core::args_ref::read_text_slice;
use crate::core::batch::{create_batch_response, run_batch, run_batch_parallel};
use crate::core::config::{ensure_path_allowed, existing_path, target_path};
use crate::core::response::RawResult;
use base64::{Engine as _, engine::general_purpose};
use serde_json::{Map, Value, json};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;
use std::time::UNIX_EPOCH;

enum SliceRead {
    Text { content: String, line_count: usize },
    Binary(Vec<u8>),
    NonAscii,
}

// 1. Read tools ---------------------------------------------------------------
pub fn handle_file_read(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let items = read_items(args);
    if items.is_empty() {
        return RawResult::error("paths or items is required");
    }

    let results = run_batch_parallel(items, |item| read_item(item, allow_missing));
    create_batch_response("file-read", results, true)
}

pub fn handle_file_lines(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let items = read_items(args);
    if items.is_empty() {
        return RawResult::error("paths or items is required");
    }

    let results = run_batch_parallel(items, |item| lines_item(item, allow_missing));
    create_batch_response("file-lines", results, true)
}

fn read_items(args: &Value) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(paths) = args.get("paths").and_then(Value::as_array) {
        items.extend(
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(|path| json!({ "path": path })),
        );
    }
    if let Some(raw_items) = args.get("items").and_then(Value::as_array) {
        items.extend(raw_items.iter().cloned());
    }

    items
}

fn read_item(item: Value, allow_missing: bool) -> RawResult {
    if bool_field(&item, "isUrl", false) {
        return read_url_item(item);
    }

    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if !path.exists() {
        if allow_missing {
            return RawResult::structured(
                format!("Missing path: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    }

    if path.is_dir() {
        return read_directory(&path);
    }

    if let Some(mime_type) = image_mime(&path) {
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return RawResult::error(format!("Failed to read {}: {error}", path.display()));
            }
        };
        return RawResult {
            content: vec![json!({
                "type": "image",
                "data": general_purpose::STANDARD.encode(&bytes),
                "mimeType": mime_type
            })],
            structured: Some(json!({
                "path": path.display().to_string(),
                "bytes": bytes.len(),
                "mimeType": mime_type
            })),
            is_error: false,
            meta: Map::new(),
        };
    }

    let offset = usize_field(&item, "offset", 0);
    let length = item
        .get("length")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    if offset > 0 || length.is_some() {
        match read_ascii_slice(&path, offset, length) {
            Ok(SliceRead::Text {
                content,
                line_count,
            }) => {
                let bytes = match fs::metadata(&path) {
                    Ok(metadata) => metadata.len(),
                    Err(error) => {
                        return RawResult::error(format!(
                            "Failed to stat {}: {error}",
                            path.display()
                        ));
                    }
                };
                return RawResult::structured(
                    format!("{}:\n{}", path.display(), content),
                    json!({
                        "path": path.display().to_string(),
                        "content": content,
                        "bytes": bytes,
                        "lineCount": line_count
                    }),
                );
            }
            Ok(SliceRead::Binary(bytes)) => return binary_result(&path, bytes),
            Ok(SliceRead::NonAscii) => {}
            Err(error) => return RawResult::error(error),
        }
    }

    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return RawResult::error(format!("Failed to read {}: {error}", path.display()));
        }
    };

    if bytes.contains(&0) {
        return binary_result(&path, bytes);
    }

    let text = String::from_utf8_lossy(&bytes).to_string();
    let sliced = slice_chars(&text, offset, length);

    RawResult::structured(
        format!("{}:\n{}", path.display(), sliced),
        json!({
            "path": path.display().to_string(),
            "content": sliced,
            "bytes": bytes.len(),
            "lineCount": text.lines().count()
        }),
    )
}

fn read_url_item(item: Value) -> RawResult {
    let Some(url) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a URL string");
    };

    let response = match fetch_http_url(url, 0) {
        Ok(response) => response,
        Err(error) => return RawResult::error(error),
    };
    let content = String::from_utf8_lossy(&response.body).to_string();
    let sliced = slice_chars(
        &content,
        usize_field(&item, "offset", 0),
        item.get("length")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
    );

    RawResult::structured(
        format!("{url}:\n{sliced}"),
        json!({
            "url": url,
            "status": response.status,
            "headers": response.headers,
            "content": sliced,
            "bytes": response.body.len()
        }),
    )
}

fn read_directory(path: &Path) -> RawResult {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            return RawResult::error(format!("Failed to list {}: {error}", path.display()));
        }
    };

    let mut names = Vec::new();
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let marker = if entry_path.is_dir() { "/" } else { "" };
        names.push(format!("{}{}", entry.file_name().to_string_lossy(), marker));
    }
    names.sort();

    RawResult::structured(
        format!("{}:\n{}", path.display(), names.join("\n")),
        json!({
            "path": path.display().to_string(),
            "entries": names,
            "directory": true
        }),
    )
}

fn lines_item(item: Value, allow_missing: bool) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if !path.exists() {
        if allow_missing {
            return RawResult::structured(
                format!("Missing path: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    }
    if path.is_dir() {
        return RawResult::error(format!("Path is a directory: {}", path.display()));
    }

    let offset = usize_field(&item, "offset", 0);
    let length = item
        .get("length")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let selected = match read_lines_native(&path, offset, length) {
        Ok(selected) => selected,
        Err(error) => {
            return RawResult::error(error);
        }
    };

    lines_result(&path, selected, Some("native-rust"))
}

fn lines_result(path: &Path, selected: Vec<(usize, String)>, backend: Option<&str>) -> RawResult {
    let numbered = selected
        .iter()
        .map(|(number, line)| format!("{number}: {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut structured = json!({
        "path": path.display().to_string(),
        "lines": selected
            .into_iter()
            .map(|(number, text)| json!({ "number": number, "text": text }))
            .collect::<Vec<_>>()
    });
    if let Some(backend) = backend {
        structured["backend"] = json!(backend);
    }

    let mut result =
        RawResult::structured(format!("{}:\n{}", path.display(), numbered), structured);
    if let Some(backend) = backend {
        result.meta.insert("backend".to_string(), json!(backend));
    }

    result
}

fn read_lines_native(
    path: &Path,
    offset: usize,
    length: Option<usize>,
) -> Result<Vec<(usize, String)>, String> {
    if length == Some(0) {
        return Ok(Vec::new());
    }

    let file = fs::File::open(path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let reader = BufReader::new(file);
    let mut selected = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        if index < offset {
            continue;
        }
        if let Some(length) = length
            && selected.len() >= length
        {
            break;
        }

        let line = line.map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        selected.push((index + 1, line));
    }
    Ok(selected)
}

// 2. Write and directory tools ------------------------------------------------
pub fn handle_file_write(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items.clone(), write_item);
    create_batch_response("file-write", results, false)
}

pub fn handle_dir_mk(args: &Value) -> RawResult {
    let Some(paths) = args.get("paths").and_then(Value::as_array) else {
        return RawResult::error("paths must be an array");
    };

    let items = paths
        .iter()
        .filter_map(Value::as_str)
        .map(|path| json!({ "path": path }))
        .collect();
    let results = run_batch(items, mkdir_item);
    create_batch_response("dir-mk", results, false)
}

pub fn handle_dir_list(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch_parallel(items.clone(), |item| list_dir_item(item, allow_missing));
    create_batch_response("dir-list", results, true)
}

fn write_item(item: Value) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let content = match item_content(&item, "content", "content_path") {
        Ok(content) => content,
        Err(error) => return RawResult::error(error),
    };
    let path = match target_path(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if let Err(error) = ensure_parent_dir(&path) {
        return RawResult::error(error);
    }

    let mode = item
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("rewrite");
    let result = if mode == "append" {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(content.as_bytes()))
    } else {
        fs::write(&path, content.as_bytes())
    };

    match result {
        Ok(()) => RawResult::structured(
            format!("Wrote {} bytes to {}", content.len(), path.display()),
            json!({
                "path": path.display().to_string(),
                "bytes": content.len(),
                "mode": mode
            }),
        ),
        Err(error) => RawResult::error(format!("Failed to write {}: {error}", path.display())),
    }
}

fn mkdir_item(item: Value) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match target_path(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    match fs::create_dir_all(&path) {
        Ok(()) => RawResult::structured(
            format!("Created directory {}", path.display()),
            json!({ "path": path.display().to_string() }),
        ),
        Err(error) => RawResult::error(format!("Failed to create {}: {error}", path.display())),
    }
}

fn list_dir_item(item: Value, allow_missing: bool) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if !path.exists() {
        if allow_missing {
            return RawResult::structured(
                format!("Missing directory: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        return RawResult::error(format!("Directory does not exist: {}", path.display()));
    }
    if !path.is_dir() {
        return RawResult::error(format!("Path is not a directory: {}", path.display()));
    }

    let depth = usize_field(&item, "depth", 2);
    let max_entries = usize_field(&item, "maxEntries", 500);
    let include_files = bool_field(&item, "includeFiles", true);
    let excludes = item
        .get("excludePatterns")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if max_entries == 0 {
        return dir_list_result(&path, Vec::new(), max_entries, None);
    }

    let (entries, backend) =
        match list_dir_native(&path, depth, max_entries, include_files, &excludes) {
            Ok(entries) => (entries, Some("native-rust")),
            Err(error) => return RawResult::error(error),
        };

    dir_list_result(&path, entries, max_entries, backend)
}

fn dir_list_result(
    path: &Path,
    entries: Vec<String>,
    max_entries: usize,
    backend: Option<&str>,
) -> RawResult {
    let text = entries.join("\n");
    let truncated = entries.len() >= max_entries;
    let mut structured = json!({
        "path": path.display().to_string(),
        "entries": entries.clone(),
        "truncated": truncated
    });
    if let Some(backend) = backend {
        structured["backend"] = json!(backend);
    }

    let mut result = RawResult::structured(format!("{}:\n{}", path.display(), text), structured);
    if let Some(backend) = backend {
        result.meta.insert("backend".to_string(), json!(backend));
    }

    result
}

fn list_dir_native(
    path: &Path,
    depth: usize,
    max_entries: usize,
    include_files: bool,
    excludes: &[String],
) -> Result<Vec<String>, String> {
    let mut entries = Vec::new();
    if depth == 0 || max_entries == 0 {
        return Ok(entries);
    }
    let excludes = ExcludeSet::new(excludes);
    let ctx = DirCollect {
        root: path,
        max_depth: depth,
        max_entries,
        include_files,
        excludes: &excludes,
    };

    collect_dir_entries(&ctx, path, 1, &mut entries)?;
    entries.sort();
    Ok(entries)
}

struct DirCollect<'a> {
    root: &'a Path,
    max_depth: usize,
    max_entries: usize,
    include_files: bool,
    excludes: &'a ExcludeSet,
}

fn collect_dir_entries(
    ctx: &DirCollect<'_>,
    dir: &Path,
    current_depth: usize,
    entries: &mut Vec<String>,
) -> Result<(), String> {
    if entries.len() >= ctx.max_entries {
        return Ok(());
    }

    let mut dir_entries = fs::read_dir(dir)
        .map_err(|error| format!("Failed to list {}: {error}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    dir_entries.sort_by_key(|entry| entry.path());

    let mut children = Vec::new();
    for entry in dir_entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        let is_dir = file_type.is_dir();
        let is_file = file_type.is_file();
        let Some(rel) = native_entry_name(ctx.root, &path, is_dir) else {
            continue;
        };
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if ctx.excludes.matches(&rel, name, is_dir) {
            continue;
        }

        if is_dir {
            entries.push(rel);
            if current_depth < ctx.max_depth {
                children.push(path);
            }
        } else if ctx.include_files && is_file {
            entries.push(rel);
        }

        if entries.len() >= ctx.max_entries {
            return Ok(());
        }
    }

    for child in children {
        collect_dir_entries(ctx, &child, current_depth + 1, entries)?;
        if entries.len() >= ctx.max_entries {
            return Ok(());
        }
    }

    Ok(())
}

struct ExcludeSet {
    patterns: Vec<String>,
}

impl ExcludeSet {
    fn new(patterns: &[String]) -> Self {
        Self {
            patterns: patterns
                .iter()
                .map(|pattern| pattern.replace('\\', "/"))
                .filter(|pattern| !pattern.trim().is_empty())
                .collect(),
        }
    }

    fn matches(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        self.patterns
            .iter()
            .any(|pattern| exclude_match(pattern, rel, name, is_dir))
    }
}

fn native_entry_name(root: &Path, path: &Path, is_dir: bool) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }

    let rel = rel.to_string_lossy().replace('\\', "/");
    if is_dir {
        Some(format!("{rel}/"))
    } else {
        Some(rel)
    }
}

fn exclude_match(pattern: &str, rel: &str, name: &str, is_dir: bool) -> bool {
    let pattern = pattern.trim();
    let dir_only = pattern.ends_with('/');
    if dir_only && !is_dir {
        return false;
    }

    let pattern = pattern.trim_matches('/');
    let rel = rel.trim_end_matches('/');
    if pattern == rel || pattern == name {
        return true;
    }
    if let Some(rest) = pattern.strip_prefix("**/")
        && (rest == name || wildcard_match(rest, name) || wildcard_match(rest, rel))
    {
        return true;
    }

    wildcard_match(pattern, rel) || wildcard_match(pattern, name)
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    let mut pattern_index = 0usize;
    let mut value_index = 0usize;
    let mut star_index = None;
    let mut star_value = 0usize;

    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star_index = Some(pattern_index);
            star_value = value_index;
            pattern_index += 1;
        } else if let Some(index) = star_index {
            pattern_index = index + 1;
            star_value += 1;
            value_index = star_value;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }

    pattern_index == pattern.len()
}

// 3. Copy, move, remove, metadata, edit --------------------------------------
pub fn handle_file_copy(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items.clone(), copy_item);
    create_batch_response("file-copy", results, false)
}

pub fn handle_file_move(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items.clone(), move_item);
    create_batch_response("file-move", results, false)
}

pub fn handle_file_remove(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items.clone(), remove_item);
    create_batch_response("file-remove", results, false)
}

pub fn handle_file_infos(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let Some(paths) = args.get("paths").and_then(Value::as_array) else {
        return RawResult::error("paths must be an array");
    };

    let items = paths
        .iter()
        .filter_map(Value::as_str)
        .map(|path| json!({ "path": path }))
        .collect();
    let results = run_batch_parallel(items, |item| info_item(item, allow_missing));
    create_batch_response("file-infos", results, false)
}

pub fn handle_file_edit(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items.clone(), edit_item);
    create_batch_response("file-edit", results, false)
}

pub fn handle_file_edit_lines(args: &Value) -> RawResult {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error("items must be an array");
    };

    let results = run_batch(items.clone(), edit_lines_item);
    create_batch_response("file-edit-lines", results, false)
}

fn copy_item(item: Value) -> RawResult {
    let Some(source) = item.get("source").and_then(Value::as_str) else {
        return RawResult::error("source must be a string");
    };
    let Some(destination) = item.get("destination").and_then(Value::as_str) else {
        return RawResult::error("destination must be a string");
    };

    let source = match existing_path(source) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let destination = match target_path(destination) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let recursive = bool_field(&item, "recursive", false);
    let force = bool_field(&item, "force", false);
    if destination.exists() && !force {
        return RawResult::error(format!("Destination exists: {}", destination.display()));
    }

    let result = if source.is_dir() {
        if !recursive {
            return RawResult::error("recursive must be true to copy a directory");
        }
        copy_dir_recursive(&source, &destination)
    } else {
        if let Err(error) = ensure_parent_dir(&destination) {
            return RawResult::error(error);
        }
        fs::copy(&source, &destination)
            .map(|_| ())
            .map_err(|error| error.to_string())
    };

    match result {
        Ok(()) => RawResult::structured(
            format!("Copied {} -> {}", source.display(), destination.display()),
            json!({
                "source": source.display().to_string(),
                "destination": destination.display().to_string()
            }),
        ),
        Err(error) => RawResult::error(format!("Failed to copy: {error}")),
    }
}

fn move_item(item: Value) -> RawResult {
    let Some(source) = item.get("source").and_then(Value::as_str) else {
        return RawResult::error("source must be a string");
    };
    let Some(destination) = item.get("destination").and_then(Value::as_str) else {
        return RawResult::error("destination must be a string");
    };

    let source = match existing_path(source) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let destination = match target_path(destination) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if let Err(error) = ensure_parent_dir(&destination) {
        return RawResult::error(error);
    }

    match fs::rename(&source, &destination) {
        Ok(()) => RawResult::structured(
            format!("Moved {} -> {}", source.display(), destination.display()),
            json!({
                "source": source.display().to_string(),
                "destination": destination.display().to_string()
            }),
        ),
        Err(error) => RawResult::error(format!("Failed to move: {error}")),
    }
}

fn remove_item(item: Value) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let force = bool_field(&item, "force", false);
    if !path.exists() {
        if force {
            return RawResult::structured(
                format!("Already absent: {}", path.display()),
                json!({ "path": path.display().to_string(), "removed": false }),
            );
        }
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    }

    let result = if path.is_dir() {
        if !bool_field(&item, "recursive", false) {
            return RawResult::error("recursive must be true to remove a directory");
        }
        fs::remove_dir_all(&path)
    } else {
        fs::remove_file(&path)
    };

    match result {
        Ok(()) => RawResult::structured(
            format!("Removed {}", path.display()),
            json!({ "path": path.display().to_string(), "removed": true }),
        ),
        Err(error) => RawResult::error(format!("Failed to remove {}: {error}", path.display())),
    }
}

fn info_item(item: Value, allow_missing: bool) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };
    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };

    if !path.exists() {
        if allow_missing {
            return RawResult::structured(
                format!("Missing path: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        return RawResult::error(format!("Path does not exist: {}", path.display()));
    }

    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return RawResult::error(format!("Failed to stat {}: {error}", path.display()));
        }
    };

    RawResult::structured(
        format!("{}: {} bytes", path.display(), metadata.len()),
        json!({
            "path": path.display().to_string(),
            "isDirectory": metadata.is_dir(),
            "isFile": metadata.is_file(),
            "len": metadata.len(),
            "readonly": metadata.permissions().readonly(),
            "modified": timestamp(metadata.modified().ok()),
            "created": timestamp(metadata.created().ok()),
            "accessed": timestamp(metadata.accessed().ok())
        }),
    )
}

fn edit_item(item: Value) -> RawResult {
    let Some(path) = item.get("file_path").and_then(Value::as_str) else {
        return RawResult::error("file_path must be a string");
    };
    let path = match existing_path(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if path.is_dir() {
        return RawResult::error(format!("Path is a directory: {}", path.display()));
    }

    let old_string = match item_content(&item, "old_string", "old_string_path") {
        Ok(content) => content,
        Err(error) => return RawResult::error(error),
    };
    let new_string = match item_content(&item, "new_string", "new_string_path") {
        Ok(content) => content,
        Err(error) => return RawResult::error(error),
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return RawResult::error(format!("Failed to read {}: {error}", path.display()));
        }
    };

    let (effective_old, effective_new, eol_mode) =
        resolve_edit_strings(&text, &old_string, &new_string);
    let count = text.matches(&effective_old).count();
    if count == 0 {
        return RawResult::error("old_string was not found");
    }
    match item.get("expected_replacements").and_then(Value::as_u64) {
        Some(expected) if count != expected as usize => {
            return RawResult::error(format!(
                "Expected {expected} replacements but found {count}"
            ));
        }
        _ => {}
    }

    let edited = text.replace(&effective_old, &effective_new);
    if let Err(error) = fs::write(&path, edited.as_bytes()) {
        return RawResult::error(format!("Failed to write {}: {error}", path.display()));
    }

    RawResult::structured(
        format!("Edited {} ({count} replacements)", path.display()),
        json!({
            "file_path": path.display().to_string(),
            "replacements": count,
            "bytes": edited.len(),
            "eolMode": eol_mode
        }),
    )
}

fn edit_lines_item(item: Value) -> RawResult {
    let Some(path) = item.get("file_path").and_then(Value::as_str) else {
        return RawResult::error("file_path must be a string");
    };
    let path = match existing_path(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    if path.is_dir() {
        return RawResult::error(format!("Path is a directory: {}", path.display()));
    }

    let Some(start_line) = item.get("start_line").and_then(Value::as_u64) else {
        return RawResult::error("start_line must be a positive integer");
    };
    if start_line == 0 {
        return RawResult::error("start_line is 1-based and must be >= 1");
    }
    let end_line = item
        .get("end_line")
        .and_then(Value::as_u64)
        .unwrap_or(start_line);
    if end_line < start_line {
        return RawResult::error("end_line must be >= start_line");
    }
    let after = bool_field(&item, "after", false);
    let replacement = match item.get("replacement").and_then(Value::as_str) {
        Some(value) => value.to_string(),
        None => match item_content(&item, "replacement", "replacement_path") {
            Ok(value) => value,
            Err(error) => return RawResult::error(error),
        },
    };

    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return RawResult::error(format!("Failed to read {}: {error}", path.display()));
        }
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => return RawResult::error("file is not valid UTF-8"),
    };

    let eol = detect_dominant_eol(text);
    let line_ranges = compute_line_ranges(text);
    let total_lines = line_ranges.len();

    if let Some(expected) = item.get("expected_lines").and_then(Value::as_u64)
        && total_lines as u64 != expected
    {
        return RawResult::error(format!(
            "Expected {expected} lines but file has {total_lines}"
        ));
    }

    if start_line as usize > total_lines && !after {
        return RawResult::error(format!(
            "start_line {start_line} exceeds file line count {total_lines}"
        ));
    }
    let effective_end = (end_line as usize).min(total_lines.max(1));
    let start_idx = start_line as usize - 1;
    let end_idx = effective_end.saturating_sub(1);

    let normalized = normalize_replacement_eol(&replacement, eol);
    let trailing_eol_needed = !normalized.is_empty() && !ends_with_eol(&normalized);
    let final_replacement = if trailing_eol_needed && (after || end_idx < total_lines) {
        let mut value = normalized;
        value.push_str(eol);
        value
    } else {
        normalized
    };

    let mut new_text = String::with_capacity(text.len() + final_replacement.len());
    if after {
        let insert_byte = if total_lines == 0 {
            0
        } else if end_idx < total_lines {
            line_ranges[end_idx].1
        } else {
            text.len()
        };
        new_text.push_str(&text[..insert_byte]);
        let needs_eol_before = insert_byte > 0
            && !text[..insert_byte].ends_with('\n')
            && !final_replacement.is_empty();
        if needs_eol_before {
            new_text.push_str(eol);
        }
        new_text.push_str(&final_replacement);
        new_text.push_str(&text[insert_byte..]);
    } else {
        let cut_start = line_ranges[start_idx].0;
        let cut_end = line_ranges[end_idx].1;
        new_text.push_str(&text[..cut_start]);
        new_text.push_str(&final_replacement);
        new_text.push_str(&text[cut_end..]);
    }

    if let Err(error) = fs::write(&path, new_text.as_bytes()) {
        return RawResult::error(format!("Failed to write {}: {error}", path.display()));
    }

    let action = if after {
        "insert_after"
    } else if final_replacement.is_empty() {
        "delete"
    } else {
        "replace"
    };
    let lines_changed = if after {
        0
    } else {
        effective_end - start_line as usize + 1
    };

    RawResult::structured(
        format!(
            "{action} {} (lines {start_line}-{effective_end})",
            path.display()
        ),
        json!({
            "file_path": path.display().to_string(),
            "action": action,
            "start_line": start_line,
            "end_line": effective_end,
            "lines_removed": lines_changed,
            "bytes": new_text.len(),
            "eol": match eol { "\r\n" => "crlf", _ => "lf" }
        }),
    )
}

fn detect_dominant_eol(text: &str) -> &'static str {
    // 단일 패스로 CRLF / LF 동시 카운트. 이전에는 text.matches() 를 두 번 호출하여
    // 전체 문자열을 2회 스캔하던 비용을 1회로 축소.
    let bytes = text.as_bytes();
    let mut crlf = 0usize;
    let mut lf_only = 0usize;
    for index in 0..bytes.len() {
        if bytes[index] != b'\n' {
            continue;
        }
        if index > 0 && bytes[index - 1] == b'\r' {
            crlf += 1;
        } else {
            lf_only += 1;
        }
    }
    if crlf >= lf_only && crlf > 0 {
        "\r\n"
    } else {
        "\n"
    }
}

fn compute_line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            ranges.push((start, index + 1));
            start = index + 1;
        }
    }
    if start < bytes.len() {
        ranges.push((start, bytes.len()));
    }
    ranges
}

fn ends_with_eol(value: &str) -> bool {
    value.ends_with('\n') || value.ends_with('\r')
}

fn normalize_replacement_eol(value: &str, target_eol: &str) -> String {
    if target_eol == "\n" {
        return crlf_to_lf(value);
    }
    let lf_only = crlf_to_lf(value);
    lf_to_crlf(&lf_only)
}

fn resolve_edit_strings(text: &str, old: &str, new: &str) -> (String, String, &'static str) {
    if text.contains(old) {
        return (old.to_string(), new.to_string(), "raw");
    }
    let file_has_crlf = text.contains("\r\n");
    let old_has_crlf = old.contains("\r\n");
    let new_has_crlf = new.contains("\r\n");
    if file_has_crlf && !old_has_crlf {
        let new_old = lf_to_crlf(old);
        if text.contains(&new_old) {
            let new_new = if new_has_crlf {
                new.to_string()
            } else {
                lf_to_crlf(new)
            };
            return (new_old, new_new, "lf_to_crlf");
        }
    }
    if !file_has_crlf && old_has_crlf {
        let new_old = crlf_to_lf(old);
        if text.contains(&new_old) {
            let new_new = if new_has_crlf { crlf_to_lf(new) } else { new.to_string() };
            return (new_old, new_new, "crlf_to_lf");
        }
    }
    (old.to_string(), new.to_string(), "raw")
}

fn lf_to_crlf(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + value.matches('\n').count());
    let mut prev_was_cr = false;
    for ch in value.chars() {
        if ch == '\n' && !prev_was_cr {
            out.push('\r');
            out.push('\n');
        } else {
            out.push(ch);
        }
        prev_was_cr = ch == '\r';
    }
    out
}

fn crlf_to_lf(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' && chars.peek() == Some(&'\n') {
            chars.next();
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

// 4. Shared helpers -----------------------------------------------------------
fn ensure_parent_dir(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create {}: {error}", parent.display()))
}

fn item_content(item: &Value, inline_key: &str, path_key: &str) -> Result<String, String> {
    if let Some(path) = item.get(path_key).and_then(Value::as_str) {
        let path = ensure_path_allowed(path)?;
        let offset_key = inline_key
            .replace("string", "string_offset")
            .replace("content", "content_offset");
        let length_key = inline_key
            .replace("string", "string_length")
            .replace("content", "content_length");
        let offset = item.get(&offset_key).and_then(Value::as_u64).unwrap_or(0) as usize;
        let length = item
            .get(&length_key)
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        return read_text_slice(path, offset, length);
    }

    item.get(inline_key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{inline_key} or {path_key} is required"))
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path)
                .map(|_| ())
                .map_err(|error| error.to_string())?;
        }
    }

    Ok(())
}

fn image_mime(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("bmp") => Some("image/bmp"),
        Some("svg") => Some("image/svg+xml"),
        _ => None,
    }
}

fn binary_result(path: &Path, bytes: Vec<u8>) -> RawResult {
    RawResult::structured(
        format!("Binary file: {} ({} bytes)", path.display(), bytes.len()),
        json!({
            "path": path.display().to_string(),
            "binary": true,
            "bytes": bytes.len(),
            "base64": general_purpose::STANDARD.encode(&bytes)
        }),
    )
}

fn timestamp(value: Option<std::time::SystemTime>) -> Option<u64> {
    value
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

fn bool_field(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn usize_field(value: &Value, key: &str, default: usize) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default)
}

fn slice_chars(text: &str, offset: usize, length: Option<usize>) -> String {
    let chars = text.chars().skip(offset);
    match length {
        Some(length) => chars.take(length).collect(),
        None => chars.collect(),
    }
}

fn read_ascii_slice(
    path: &Path,
    offset: usize,
    length: Option<usize>,
) -> Result<SliceRead, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let mut buffer = [0u8; 64 * 1024];
    let mut content = String::with_capacity(length.unwrap_or(0).min(1024 * 1024));
    let mut char_index = 0usize;
    let limit = length
        .map(|length| offset.saturating_add(length))
        .unwrap_or(usize::MAX);
    let mut line_breaks = 0usize;
    let mut saw_text = false;
    let mut last_was_lf = false;

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }

        let chunk = &buffer[..read];
        for byte in chunk {
            if *byte == 0 {
                let bytes = fs::read(path)
                    .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
                return Ok(SliceRead::Binary(bytes));
            }
            if *byte >= 0x80 {
                return Ok(SliceRead::NonAscii);
            }
        }

        if !chunk.is_empty() {
            saw_text = true;
            last_was_lf = chunk.last() == Some(&b'\n');
            line_breaks += chunk.iter().filter(|byte| **byte == b'\n').count();
        }

        let chunk_start = char_index;
        let chunk_end = char_index + read;
        if chunk_end > offset && chunk_start < limit {
            let start = offset.saturating_sub(chunk_start);
            let end = (limit.min(chunk_end)) - chunk_start;
            let text = std::str::from_utf8(&chunk[start..end])
                .map_err(|error| format!("Failed to decode {}: {error}", path.display()))?;
            content.push_str(text);
        }
        char_index = chunk_end;
    }

    let line_count = line_breaks + usize::from(saw_text && !last_was_lf);
    Ok(SliceRead::Text {
        content,
        line_count,
    })
}

struct HttpResponse {
    status: u16,
    headers: Vec<String>,
    body: Vec<u8>,
}

fn fetch_http_url(url: &str, redirects: usize) -> Result<HttpResponse, String> {
    if redirects > 5 {
        return Err("Too many HTTP redirects".to_string());
    }
    let parsed = parse_http_url(url)?;
    let mut stream = TcpStream::connect((&*parsed.host, parsed.port))
        .map_err(|error| format!("Failed to connect to {}: {error}", parsed.host))?;
    let timeout = Some(Duration::from_secs(20));
    stream
        .set_read_timeout(timeout)
        .map_err(|error| format!("Failed to set read timeout: {error}"))?;
    stream
        .set_write_timeout(timeout)
        .map_err(|error| format!("Failed to set write timeout: {error}"))?;

    let host_header = if parsed.port == 80 {
        parsed.host.clone()
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: rust-fs-mcp/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        parsed.path,
        host_header,
        env!("CARGO_PKG_VERSION")
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("Failed to write HTTP request: {error}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| format!("Failed to read HTTP response: {error}"))?;
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "Invalid HTTP response: missing header terminator".to_string())?;
    let header_text = String::from_utf8_lossy(&response[..header_end]);
    let mut header_lines = header_text.lines();
    let status_line = header_lines
        .next()
        .ok_or_else(|| "Invalid HTTP response: missing status line".to_string())?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| format!("Invalid HTTP status line: {status_line}"))?;
    let headers = header_lines.map(str::to_string).collect::<Vec<_>>();
    let location = if matches!(status, 301 | 302 | 303 | 307 | 308) {
        redirect_location(&headers)
    } else {
        None
    };
    if let Some(location) = location {
        let next_url = if location.starts_with("http://") || location.starts_with("https://") {
            location
        } else {
            format!("http://{}{}", parsed.host, location)
        };
        return fetch_http_url(&next_url, redirects + 1);
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP request failed with status {status}"));
    }

    Ok(HttpResponse {
        status,
        headers,
        body: response[header_end + 4..].to_vec(),
    })
}

struct ParsedHttpUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Result<ParsedHttpUrl, String> {
    if url.starts_with("https://") {
        return Err("HTTPS URL reads require a TLS-capable Rust HTTP client layer".to_string());
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return Err("Only http:// and https:// URL schemes are accepted".to_string());
    };
    let (authority, path) = rest
        .split_once('/')
        .map(|(authority, path)| (authority, format!("/{path}")))
        .unwrap_or((rest, "/".to_string()));
    if authority.is_empty() {
        return Err("HTTP URL host is required".to_string());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|ch| ch.is_ascii_digit()) => {
            let port = port
                .parse::<u16>()
                .map_err(|error| format!("Invalid HTTP port: {error}"))?;
            (host.to_string(), port)
        }
        _ => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err("HTTP URL host is required".to_string());
    }

    Ok(ParsedHttpUrl { host, port, path })
}

fn redirect_location(headers: &[String]) -> Option<String> {
    headers.iter().find_map(|header| {
        let (name, value) = header.split_once(':')?;
        if name.eq_ignore_ascii_case("location") {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn edit_lines_replaces_single_line_crlf_preserved() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-replace");
        let _guard = edit_lines_lock();
        let path = dir.join("sample.txt");
        let initial = (1..=20)
            .map(|index| format!("line {index}\r\n"))
            .collect::<String>();
        std::fs::write(&path, initial).unwrap();

        let result = edit_lines_item(json!({
            "file_path": path.display().to_string(),
            "start_line": 15,
            "end_line": 15,
            "replacement": "line 15 REPLACED"
        }));
        assert!(!result.is_error, "{result:?}");
        let edited = std::fs::read_to_string(&path).unwrap();
        assert!(edited.contains("line 14\r\nline 15 REPLACED\r\nline 16\r\n"));
        assert!(!edited.contains("line 15\r\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_lines_deletes_range() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-delete");
        let _guard = edit_lines_lock();
        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();

        let result = edit_lines_item(json!({
            "file_path": path.display().to_string(),
            "start_line": 3,
            "end_line": 4,
            "replacement": ""
        }));
        assert!(!result.is_error, "{result:?}");
        let edited = std::fs::read_to_string(&path).unwrap();
        assert_eq!(edited, "a\nb\ne\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_lines_inserts_after_line() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-insert");
        let _guard = edit_lines_lock();
        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();

        let result = edit_lines_item(json!({
            "file_path": path.display().to_string(),
            "start_line": 2,
            "end_line": 2,
            "replacement": "INSERTED",
            "after": true
        }));
        assert!(!result.is_error, "{result:?}");
        let edited = std::fs::read_to_string(&path).unwrap();
        assert_eq!(edited, "a\nb\nINSERTED\nc\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_lines_rejects_out_of_range() {
        let dir = make_temp_dir("rust-fs-mcp-edit-lines-oor");
        let _guard = edit_lines_lock();
        let path = dir.join("sample.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let result = edit_lines_item(json!({
            "file_path": path.display().to_string(),
            "start_line": 99,
            "replacement": "X"
        }));
        assert!(result.is_error, "{result:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn config_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::Mutex;
        use std::sync::OnceLock;
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    fn edit_lines_lock() -> std::sync::MutexGuard<'static, ()> {
        let guard = config_lock();
        let target = std::env::current_dir().unwrap();
        crate::core::config::handle_set_config_values(&json!({
            "items": [{
                "key": "allowedDirectories",
                "value": [target.display().to_string()]
            }]
        }));
        guard
    }

    fn make_temp_dir(prefix: &str) -> std::path::PathBuf {
        let dir = std::env::current_dir().unwrap().join("target").join(format!(
            "{prefix}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_edit_matches_across_crlf_lf_mismatch() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-edit-eol-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        crate::core::config::handle_set_config_values(&json!({
            "items": [{
                "key": "allowedDirectories",
                "value": [dir.display().to_string()]
            }]
        }));
        let path = dir.join("sample.txt");
        std::fs::write(&path, "line 19\r\nline 20\r\nline 21\r\n").unwrap();

        let result = edit_item(json!({
            "file_path": path.display().to_string(),
            "old_string": "line 20\n",
            "new_string": "",
            "expected_replacements": 1
        }));
        assert!(!result.is_error, "{result:?}");
        let edited = std::fs::read_to_string(&path).unwrap();
        assert_eq!(edited, "line 19\r\nline 21\r\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_and_reads_file() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-test-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        let _guard = config_lock();
        let project = std::env::current_dir().unwrap();
        crate::core::config::handle_set_config_values(&json!({
            "items": [{
                "key": "allowedDirectories",
                "value": [project.display().to_string()]
            }]
        }));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.txt");
        let write = handle_file_write(&json!({
            "items": [{ "path": path.display().to_string(), "content": "abc" }]
        }));
        assert!(!write.is_error, "{write:?}");

        let read = handle_file_read(&json!({ "paths": [path.display().to_string()] }));
        assert!(!read.is_error);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dir_list_excludes_with_native_backend() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-dir-list-test-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        let _guard = config_lock();
        let project = std::env::current_dir().unwrap();
        crate::core::config::handle_set_config_values(&json!({
            "items": [{
                "key": "allowedDirectories",
                "value": [project.display().to_string()]
            }]
        }));
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("target").join("skip.txt"), "skip").unwrap();
        fs::write(dir.join("src").join("keep.txt"), "keep").unwrap();

        let result = handle_dir_list(&json!({
            "items": [{
                "path": dir.display().to_string(),
                "depth": 2,
                "includeFiles": true,
                "excludePatterns": ["target"]
            }]
        }));
        assert!(!result.is_error, "{result:?}");
        let structured = result.structured.unwrap();
        let inner = &structured["results"][0]["result"]["structuredContent"];
        let entries = inner["entries"].as_array().unwrap();
        assert_eq!(inner["backend"], "native-rust");
        assert!(entries.iter().any(|entry| entry == "src/"));
        assert!(entries.iter().all(|entry| {
            let entry = entry.as_str().unwrap();
            !entry.starts_with("target")
        }));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_http_url() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                .unwrap();
        });

        let read = handle_file_read(&json!({
            "items": [{ "path": format!("http://{addr}/"), "isUrl": true }]
        }));
        assert!(!read.is_error, "{read:?}");
    }
}
