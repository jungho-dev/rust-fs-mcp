//! dir.rs
//! tools::fs::dir
//!
//! dir-create and dir-list handlers with the native recursive walk and exclude-pattern matching.
//!

use super::shared::{path_touched, usize_field};
use crate::core::batch::{
    create_batch_response, parallel_plan, run_batch_mutation, run_batch_parallel,
};
use crate::core::config::{ensure_path_allowed, target_path};
use crate::core::response::RawResult;
use crate::tools::common::{bool_field, path_in_heavy_dir};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

// 1. Directory tools ----------------------------------------------------------
pub fn handle_dir_create(args: &Value) -> RawResult {
    let Some(paths) = args.get("paths").and_then(Value::as_array) else {
        return RawResult::error("paths must be an array (e.g. paths:[\"C:/absolute/path\"])");
    };

    let items = paths
        .iter()
        .filter_map(Value::as_str)
        .map(|path| json!({ "path": path }))
        .collect::<Vec<_>>();
    let results = run_batch_mutation(&items, path_touched, mkdir_item);
    create_batch_response("dir-create", results, false)
}
pub fn handle_dir_list(args: &Value) -> RawResult {
    let allow_missing = bool_field(args, "allowMissing", false);
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return RawResult::error(
            "items must be an array; wrap a single operation as items:[{...}]",
        );
    };

    let results = run_batch_parallel(items, parallel_plan(), move |item| {
        list_dir_item(item, allow_missing)
    });
    create_batch_response("dir-list", results, true)
}
fn mkdir_item(item: &Value) -> RawResult {
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
fn list_dir_item(item: &Value, allow_missing: bool) -> RawResult {
    let Some(path) = item.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };
    let Ok(metadata) = fs::metadata(&path) else {
        if allow_missing {
            return RawResult::structured(
                format!("Missing directory: {}", path.display()),
                json!({ "path": path.display().to_string(), "missing": true }),
            );
        }
        return RawResult::error(format!("Directory does not exist: {}", path.display()));
    };
    if !metadata.is_dir() {
        return RawResult::error(format!("Path is not a directory: {}", path.display()));
    }
    let depth = usize_field(item, "depth", 2);
    // No default cap: an omitted maxEntries returns every entry within depth.
    let max_entries = usize_field(item, "maxEntries", usize::MAX);
    let include_files = bool_field(item, "includeFiles", true);
    let mut excludes = item
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
    // fs-search와 동일한 기본 제외: 대형 산출물 디렉터리는 리스팅 노이즈와
    // maxEntries 소진의 주범. 대상 경로가 그 내부이거나 noDefaultExcludes:true면 끈다.
    if !bool_field(item, "noDefaultExcludes", false) && !path_in_heavy_dir(&path) {
        excludes.extend(["node_modules/", "target/", ".git/"].map(str::to_string));
    }
    if max_entries == 0 {
        return dir_list_result(&path, Vec::new(), true, None);
    }
    // Collect max+1 entries so "exactly max" and "actually truncated" are distinguishable
    // (the previous len >= max check reported a false positive at the boundary).
    let (mut entries, backend) = match list_dir_native(
        &path,
        depth,
        max_entries.saturating_add(1),
        include_files,
        &excludes,
    ) {
        Ok(entries) => (entries, Some("native-rust")),
        Err(error) => return RawResult::error(error),
    };
    let truncated = entries.len() > max_entries;
    entries.truncate(max_entries);

    dir_list_result(&path, entries, truncated, backend)
}
fn dir_list_result(
    path: &Path,
    entries: Vec<String>,
    truncated: bool,
    backend: Option<&str>,
) -> RawResult {
    // The entry list ships in the text body once; structured carries metadata only instead of
    // re-sending every entry as a JSON array.
    let cap = entries.iter().map(|item| item.len() + 1).sum::<usize>();
    let mut text = String::with_capacity(cap);
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str(entry);
    }
    let mut structured = json!({
        "path": path.display().to_string(),
        "entryCount": entries.len(),
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
// 2. Native directory walk ----------------------------------------------------
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
    // 전역 정렬(list_dir_native)이 최종 순서를 잡으므로, 절단이 없을 때(maxEntries 무제한)는
    // 디렉터리별 정렬을 건너뛴다(이중 정렬 제거).
    if ctx.max_entries != usize::MAX {
        dir_entries.sort_by_cached_key(|entry| entry.file_name());
    }

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
fn native_entry_name(root: &Path, path: &Path, is_dir: bool) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    let cow = rel.to_string_lossy();
    let needs_replace = cfg!(windows) && cow.contains('\\');
    let extra = if is_dir { 1 } else { 0 };
    let mut out = String::with_capacity(cow.len() + extra);
    if needs_replace {
        for ch in cow.chars() {
            out.push(if ch == '\\' { '/' } else { ch });
        }
    } else {
        out.push_str(&cow);
    }
    if is_dir {
        out.push('/');
    }
    Some(out)
}
// 3. Exclude pattern matching -------------------------------------------------
struct ExcludeSet {
    patterns: Vec<ExcludePattern>,
}
impl ExcludeSet {
    fn new(patterns: &[String]) -> Self {
        Self {
            patterns: patterns
                .iter()
                .map(|pattern| pattern.replace('\\', "/"))
                .filter(|pattern| !pattern.trim().is_empty())
                .map(ExcludePattern::new)
                .collect(),
        }
    }
    fn matches(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        self.patterns
            .iter()
            .any(|pattern| pattern.matches(rel, name, is_dir))
    }
}
struct ExcludePattern {
    text: String,
    dir_only: bool,
    glob: CompiledWildcard,
    rest: Option<ExcludeRest>,
}
struct ExcludeRest {
    text: String,
    glob: CompiledWildcard,
}
impl ExcludePattern {
    fn new(pattern: String) -> Self {
        let pattern = pattern.trim().to_string();
        let dir_only = pattern.ends_with('/');
        let text = pattern.trim_matches('/').to_string();
        let rest = text.strip_prefix("**/").map(|rest| ExcludeRest {
            text: rest.to_string(),
            glob: CompiledWildcard::new(rest),
        });
        Self {
            glob: CompiledWildcard::new(&text),
            text,
            dir_only,
            rest,
        }
    }
    fn matches(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let rel = rel.trim_end_matches('/');
        if self.text == rel || self.text == name {
            return true;
        }
        if let Some(rest) = &self.rest
            && (rest.text == name || rest.glob.matches(name) || rest.glob.matches(rel))
        {
            return true;
        }
        self.glob.matches(rel) || self.glob.matches(name)
    }
}
struct CompiledWildcard {
    pattern_chars: Vec<char>,
    // When the pattern is all ASCII compare against byte slices directly without a per-match `Vec<char>` allocation.
    pattern_bytes: Option<Vec<u8>>,
}
impl CompiledWildcard {
    fn new(pattern: &str) -> Self {
        let pattern_bytes = pattern.is_ascii().then(|| pattern.as_bytes().to_vec());
        Self {
            pattern_chars: pattern.chars().collect(),
            pattern_bytes,
        }
    }
    fn matches(&self, value: &str) -> bool {
        if let Some(bytes) = &self.pattern_bytes
            && value.is_ascii()
        {
            return match_glob_bytes(bytes, value.as_bytes());
        }
        let value_chars: Vec<char> = value.chars().collect();
        match_glob_chars(&self.pattern_chars, &value_chars)
    }
}
fn match_glob_bytes(pattern: &[u8], value: &[u8]) -> bool {
    let mut pi = 0usize;
    let mut vi = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_vi = 0usize;

    while vi < value.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == value[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = Some(pi);
            star_vi = vi;
            pi += 1;
        } else if let Some(index) = star_pi {
            pi = index + 1;
            star_vi += 1;
            vi = star_vi;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}
fn match_glob_chars(pattern: &[char], value: &[char]) -> bool {
    let mut pi = 0usize;
    let mut vi = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_vi = 0usize;

    while vi < value.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == value[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star_pi = Some(pi);
            star_vi = vi;
            pi += 1;
        } else if let Some(index) = star_pi {
            pi = index + 1;
            star_vi += 1;
            vi = star_vi;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::fs::make_temp_dir;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn dir_list_truncated_only_when_entries_exceed_max() {
        let dir = make_temp_dir("rust-fs-mcp-dirlist-trunc");
        for index in 0..3 {
            std::fs::write(dir.join(format!("f{index}.txt")), "x").unwrap();
        }
        let exact = list_dir_item(
            &json!({ "path": dir.display().to_string(), "maxEntries": 3 }),
            false,
        );
        let exact_meta = exact.structured.as_ref().unwrap();
        assert_eq!(exact_meta["truncated"], false, "{exact_meta:?}");
        assert_eq!(exact_meta["entryCount"], 3);
        let capped = list_dir_item(
            &json!({ "path": dir.display().to_string(), "maxEntries": 2 }),
            false,
        );
        let capped_meta = capped.structured.as_ref().unwrap();
        assert_eq!(capped_meta["truncated"], true, "{capped_meta:?}");
        assert_eq!(capped_meta["entryCount"], 2);
        std::fs::remove_dir_all(&dir).unwrap();
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
        // Compact batch entries are {index, ok, data}; the entry list itself lives in text.
        let inner = &structured["results"][0]["data"];
        assert_eq!(inner["backend"], "native-rust");
        assert_eq!(inner["entryCount"], 2);
        let text = result.content[0]["text"].as_str().unwrap();
        // 루트 경로 제거 후 엔트리 검사
        let entries = text.replace(&dir.display().to_string(), "");
        assert!(entries.contains("src/"), "{text}");
        assert!(entries.contains("src/keep.txt"), "{text}");
        assert!(!entries.contains("target"), "{text}");

        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn dir_list_hides_heavy_dirs_by_default() {
        // 시작 경로가 heavy dir 밖일 때만 기본 제외가 주입되므로 target/ 하위가 아닌
        // 시스템 temp에 만들고, noDefaultExcludes:true로 다시 보이는 것까지 확인한다.
        let dir = std::env::temp_dir().join(format!(
            "rust-fs-mcp-dirlist-heavy-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("node_modules")).unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();

        let hidden = handle_dir_list(&json!({
            "items": [{ "path": dir.display().to_string(), "depth": 2 }]
        }));
        assert!(!hidden.is_error, "{hidden:?}");
        let text = hidden.content[0]["text"].as_str().unwrap();
        assert!(text.contains("src/"), "{text}");
        assert!(!text.contains("node_modules"), "{text}");
        assert!(!text.contains(".git"), "{text}");

        let listed = handle_dir_list(&json!({
            "items": [{ "path": dir.display().to_string(), "depth": 2, "noDefaultExcludes": true }]
        }));
        assert!(!listed.is_error, "{listed:?}");
        let text = listed.content[0]["text"].as_str().unwrap();
        assert!(text.contains("node_modules/"), "{text}");
        assert!(text.contains(".git/"), "{text}");

        fs::remove_dir_all(&dir).unwrap();
    }
}
