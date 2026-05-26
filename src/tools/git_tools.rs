use crate::core::args_ref::read_text_slice;
use crate::core::config::ensure_path_allowed;
use crate::core::response::RawResult;
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

static GIT_CWD: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static PACK_CACHE: OnceLock<RwLock<HashMap<PathBuf, Arc<PackSet>>>> = OnceLock::new();
static AUTOCRLF_CACHE: OnceLock<RwLock<HashMap<PathBuf, bool>>> = OnceLock::new();
static INDEX_CACHE: OnceLock<RwLock<HashMap<PathBuf, IndexCacheEntry>>> = OnceLock::new();
static OBJECT_CACHE: OnceLock<Mutex<ObjectCache>> = OnceLock::new();
static OBJECT_DIR_CACHE: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

const OBJECT_CACHE_MAX_ITEMS: usize = 512;
const OBJECT_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const DIFF_MYERS_MAX_PRODUCT: usize = 5_000_000;

#[derive(Clone)]
struct IndexCacheEntry {
    mtime: SystemTime,
    size: u64,
    entries: Arc<Vec<IndexEntry>>,
}

#[derive(Clone, Debug)]
struct GitRepo {
    worktree: PathBuf,
    git_dir: PathBuf,
}

#[derive(Clone, Debug)]
struct IndexEntry {
    path: String,
    oid: [u8; 20],
    mode: u32,
    size: u32,
    mtime_sec: u32,
    mtime_nsec: u32,
}

#[derive(Clone, Debug)]
struct GitObject {
    kind: String,
    data: Vec<u8>,
}

#[derive(Default)]
struct ObjectCache {
    entries: HashMap<[u8; 20], Arc<GitObject>>,
    order: VecDeque<[u8; 20]>,
    bytes: usize,
}

impl ObjectCache {
    fn get(&mut self, oid: &[u8; 20]) -> Option<Arc<GitObject>> {
        let object = self.entries.get(oid).cloned()?;
        self.touch(oid);
        Some(object)
    }

    fn insert(&mut self, oid: [u8; 20], object: Arc<GitObject>) {
        let size = object_size(&object);
        if size > OBJECT_CACHE_MAX_BYTES {
            return;
        }
        if let Some(previous) = self.entries.remove(&oid) {
            self.bytes = self.bytes.saturating_sub(object_size(&previous));
            self.remove_order(&oid);
        }
        self.bytes = self.bytes.saturating_add(size);
        self.entries.insert(oid, object);
        self.order.push_back(oid);
        self.trim();
    }

    fn touch(&mut self, oid: &[u8; 20]) {
        self.remove_order(oid);
        self.order.push_back(*oid);
    }

    fn remove_order(&mut self, oid: &[u8; 20]) {
        if let Some(index) = self.order.iter().position(|item| item == oid) {
            self.order.remove(index);
        }
    }

    fn trim(&mut self) {
        while self.entries.len() > OBJECT_CACHE_MAX_ITEMS || self.bytes > OBJECT_CACHE_MAX_BYTES {
            let Some(oid) = self.order.pop_front() else {
                break;
            };
            if let Some(object) = self.entries.remove(&oid) {
                self.bytes = self.bytes.saturating_sub(object_size(&object));
            }
        }
    }
}

#[derive(Default)]
struct TreeNode {
    dirs: BTreeMap<String, TreeNode>,
    files: BTreeMap<String, IndexEntry>,
}

#[derive(Clone, Debug)]
struct TreeEntry {
    mode: String,
    name: String,
    oid: [u8; 20],
}

#[derive(Clone, Debug)]
struct DiffChange {
    path: String,
    old: Option<Vec<u8>>,
    new: Option<Vec<u8>>,
}

// 1. Git tools ----------------------------------------------------------------
pub fn handle_git_cwd(args: &Value) -> RawResult {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return RawResult::error("path must be a string");
    };

    let path = match ensure_path_allowed(path) {
        Ok(path) => path,
        Err(error) => return RawResult::error(error),
    };

    let should_init =
        bool_field(args, "initializeIfNotPresent", false) && !path.join(".git").exists();
    let repo = if should_init {
        match init_repo(&path) {
            Ok(repo) => repo,
            Err(error) => return RawResult::error(error),
        }
    } else {
        match discover_repo(&path) {
            Ok(repo) => repo,
            Err(error) if bool_field(args, "validateGitRepo", true) => {
                return RawResult::error(error);
            }
            Err(_) => {
                *git_cwd().lock().unwrap() = Some(path.clone());
                return RawResult::structured(
                    format!("Git cwd set to {}", path.display()),
                    json!({ "path": path.display().to_string(), "validated": false }),
                );
            }
        }
    };

    *git_cwd().lock().unwrap() = Some(repo.worktree.clone());
    let status = status_text(&repo, true).unwrap_or_default();
    RawResult::structured(
        format!("Git cwd set to {}", repo.worktree.display()),
        json!({
            "path": repo.worktree.display().to_string(),
            "gitDir": repo.git_dir.display().to_string(),
            "status": status
        }),
    )
}

pub fn handle_git_status(args: &Value) -> RawResult {
    let repo = match open_repo(args) {
        Ok(repo) => repo,
        Err(error) => return RawResult::error(error),
    };

    match status_text(&repo, bool_field(args, "includeUntracked", true)) {
        Ok(status) => RawResult::structured(
            status.clone(),
            json!({
                "path": repo.worktree.display().to_string(),
                "status": status,
                "entries": status.lines().collect::<Vec<_>>()
            }),
        ),
        Err(error) => RawResult::error(error),
    }
}

pub fn handle_git_add(args: &Value) -> RawResult {
    let repo = match open_repo(args) {
        Ok(repo) => repo,
        Err(error) => return RawResult::error(error),
    };
    let entries = match add_paths(&repo, args) {
        Ok(entries) => entries,
        Err(error) => return RawResult::error(error),
    };

    RawResult::structured(
        format!("Updated index with {} entries", entries.len()),
        json!({
            "path": repo.worktree.display().to_string(),
            "entries": entries.len()
        }),
    )
}

pub fn handle_git_commit(args: &Value) -> RawResult {
    let repo = match open_repo(args) {
        Ok(repo) => repo,
        Err(error) => return RawResult::error(error),
    };

    if args.get("filesToStage").is_some() {
        let add_args = json!({ "paths": string_array(args, "filesToStage").unwrap_or_default() });
        match add_paths(&repo, &add_args) {
            Ok(_) => {}
            Err(error) => return RawResult::error(error),
        }
    }

    let message = match commit_message(args) {
        Ok(message) => message,
        Err(error) => return RawResult::error(error),
    };
    if !looks_conventional(&message) {
        return RawResult::error(
            "Commit message must start with an English Conventional Commit header",
        );
    }

    let entries = match read_index(&repo) {
        Ok(entries) => entries,
        Err(error) => return RawResult::error(error),
    };
    let tree_oid = match write_tree(&repo, &entries) {
        Ok(oid) => oid,
        Err(error) => return RawResult::error(error),
    };
    let head_oid = read_head_oid(&repo).ok().flatten();
    let parent_oids = if bool_field(args, "amend", false) {
        head_oid
            .and_then(|oid| read_commit_parents(&repo, &oid).ok())
            .unwrap_or_default()
    } else {
        head_oid.into_iter().collect()
    };
    if !bool_field(args, "allowEmpty", false) {
        match parent_oids
            .first()
            .and_then(|parent| read_commit_tree(&repo, parent).ok().flatten())
        {
            Some(parent_tree) if parent_tree == tree_oid => {
                return RawResult::error("No staged changes to commit");
            }
            _ => {}
        }
    }

    let signature = signature(args);
    let commit_oid = match write_commit(&repo, tree_oid, &parent_oids, &signature, &message) {
        Ok(oid) => oid,
        Err(error) => return RawResult::error(error),
    };
    if let Err(error) = update_head(&repo, &commit_oid) {
        return RawResult::error(error);
    }

    RawResult::structured(
        format!("[{}] {}", oid_hex(&commit_oid), first_line(&message)),
        json!({
            "path": repo.worktree.display().to_string(),
            "oid": oid_hex(&commit_oid),
            "message": message
        }),
    )
}

pub fn handle_git_diff(args: &Value) -> RawResult {
    let repo = match open_repo(args) {
        Ok(repo) => repo,
        Err(error) => return RawResult::error(error),
    };
    let changes = match diff_changes(&repo, args) {
        Ok(changes) => changes,
        Err(error) => return RawResult::error(error),
    };
    let output = if bool_field(args, "nameOnly", false) {
        changes
            .iter()
            .map(|change| change.path.clone())
            .collect::<Vec<_>>()
            .join("\n")
    } else if bool_field(args, "stat", false) {
        diff_stat(&changes)
    } else {
        diff_patch(&changes)
    };

    RawResult::structured(
        output.clone(),
        json!({
            "path": repo.worktree.display().to_string(),
            "diff": output
        }),
    )
}

pub fn handle_git_show(args: &Value) -> RawResult {
    let repo = match open_repo(args) {
        Ok(repo) => repo,
        Err(error) => return RawResult::error(error),
    };
    let Some(object) = args.get("object").and_then(Value::as_str) else {
        return RawResult::error("object must be a string");
    };

    let output = match show_object(&repo, object, args.get("filePath").and_then(Value::as_str)) {
        Ok(output) => output,
        Err(error) => return RawResult::error(error),
    };
    RawResult::structured(
        output.clone(),
        json!({
            "path": repo.worktree.display().to_string(),
            "object": object,
            "output": output
        }),
    )
}

// 2. Repository discovery -----------------------------------------------------
fn open_repo(args: &Value) -> Result<GitRepo, String> {
    let path = if let Some(path) = args.get("path").and_then(Value::as_str) {
        ensure_path_allowed(path)?
    } else if let Some(path) = git_cwd().lock().unwrap().clone() {
        path
    } else {
        ensure_path_allowed(".")?
    };
    discover_repo(&path)
}

fn discover_repo(path: &Path) -> Result<GitRepo, String> {
    let mut cursor = if path.is_file() {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    };

    loop {
        let dot_git = cursor.join(".git");
        if dot_git.is_dir() {
            return Ok(GitRepo {
                worktree: cursor,
                git_dir: dot_git,
            });
        }
        if dot_git.is_file() {
            let text = fs::read_to_string(&dot_git)
                .map_err(|error| format!("Failed to read {}: {error}", dot_git.display()))?;
            if let Some(git_dir) = text.trim().strip_prefix("gitdir:") {
                let git_dir = cursor.join(git_dir.trim());
                return Ok(GitRepo {
                    worktree: cursor,
                    git_dir,
                });
            }
        }
        if !cursor.pop() {
            break;
        }
    }

    Err(format!("Git repository not found from {}", path.display()))
}

fn init_repo(path: &Path) -> Result<GitRepo, String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Failed to create {}: {error}", path.display()))?;
    let git_dir = path.join(".git");
    fs::create_dir_all(git_dir.join("objects"))
        .map_err(|error| format!("Failed to create objects directory: {error}"))?;
    fs::create_dir_all(git_dir.join("refs").join("heads"))
        .map_err(|error| format!("Failed to create refs directory: {error}"))?;
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
        .map_err(|error| format!("Failed to write HEAD: {error}"))?;
    fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = false\n",
    )
    .map_err(|error| format!("Failed to write config: {error}"))?;
    Ok(GitRepo {
        worktree: path.to_path_buf(),
        git_dir,
    })
}

fn git_cwd() -> &'static Mutex<Option<PathBuf>> {
    GIT_CWD.get_or_init(|| Mutex::new(None))
}

// 3. Index and add ------------------------------------------------------------
fn add_paths(repo: &GitRepo, args: &Value) -> Result<Vec<IndexEntry>, String> {
    let mut entries = read_index(repo)?;
    let mut paths = Vec::new();
    if bool_field(args, "all", false) {
        paths = worktree_files(repo)?;
        let live = paths
            .iter()
            .map(|path| to_repo_path(repo, path))
            .collect::<Result<BTreeSet<_>, _>>()?;
        entries.retain(|entry| live.contains(&entry.path));
    } else if bool_field(args, "update", false) {
        paths = entries
            .iter()
            .map(|entry| repo.worktree.join(&entry.path))
            .filter(|path| path.exists())
            .collect();
        let live = paths
            .iter()
            .map(|path| to_repo_path(repo, path))
            .collect::<Result<BTreeSet<_>, _>>()?;
        entries.retain(|entry| live.contains(&entry.path));
    } else {
        if let Some(path) = args.get("path").and_then(Value::as_str) {
            paths.extend(expand_add_path(repo, path)?);
        }
        if let Some(items) = string_array(args, "paths") {
            for path in items {
                paths.extend(expand_add_path(repo, &path)?);
            }
        }
        if paths.is_empty() {
            return Err("path, paths, all, or update is required".to_string());
        }
    }

    for path in paths {
        let repo_path = to_repo_path(repo, &path)?;
        if !path.exists() {
            entries.retain(|entry| entry.path != repo_path);
            continue;
        }
        if path.is_dir() {
            continue;
        }
        let entry = index_entry_for_file(repo, &path)?;
        entries.retain(|item| item.path != entry.path);
        entries.push(entry);
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    write_index(repo, &entries)?;
    Ok(entries)
}

fn read_index(repo: &GitRepo) -> Result<Vec<IndexEntry>, String> {
    let path = repo.git_dir.join("index");
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(Vec::new()),
    };
    let mtime = metadata.modified().unwrap_or(UNIX_EPOCH);
    let size = metadata.len();

    let cache = INDEX_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    {
        let map = cache.read().unwrap();
        if let Some(entry) = map.get(&repo.git_dir)
            && entry.mtime == mtime
            && entry.size == size
        {
            return Ok((*entry.entries).clone());
        }
    }

    let entries = parse_index_file(&path)?;
    let cached = IndexCacheEntry {
        mtime,
        size,
        entries: Arc::new(entries.clone()),
    };
    cache.write().unwrap().insert(repo.git_dir.clone(), cached);
    Ok(entries)
}

fn parse_index_file(path: &Path) -> Result<Vec<IndexEntry>, String> {
    let data = fs::read(path).map_err(|error| format!("Failed to read index: {error}"))?;
    if data.len() < 12 || &data[0..4] != b"DIRC" {
        return Err("Unsupported git index file".to_string());
    }
    let version = be_u32(&data[4..8]);
    if version != 2 {
        return Err(format!("Unsupported git index version: {version}"));
    }
    let count = be_u32(&data[8..12]) as usize;
    let mut offset = 12usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if offset + 62 > data.len() {
            return Err("Truncated git index entry".to_string());
        }
        let mtime_sec = be_u32(&data[offset + 8..offset + 12]);
        let mtime_nsec = be_u32(&data[offset + 12..offset + 16]);
        let mode = be_u32(&data[offset + 24..offset + 28]);
        let size = be_u32(&data[offset + 36..offset + 40]);
        let mut oid = [0u8; 20];
        oid.copy_from_slice(&data[offset + 40..offset + 60]);
        let flags = be_u16(&data[offset + 60..offset + 62]);
        let path_start = offset + 62;
        let path_len = (flags & 0x0fff) as usize;
        let path_end = if path_len < 0x0fff && path_start + path_len <= data.len() {
            path_start + path_len
        } else {
            data[path_start..]
                .iter()
                .position(|byte| *byte == 0)
                .map(|position| path_start + position)
                .ok_or_else(|| "Invalid git index path".to_string())?
        };
        let entry_path = String::from_utf8_lossy(&data[path_start..path_end]).to_string();
        entries.push(IndexEntry {
            path: entry_path,
            oid,
            mode,
            size,
            mtime_sec,
            mtime_nsec,
        });
        let entry_len = path_end + 1 - offset;
        offset += entry_len.div_ceil(8) * 8;
    }
    Ok(entries)
}

fn write_index(repo: &GitRepo, entries: &[IndexEntry]) -> Result<(), String> {
    let mut data = Vec::new();
    data.extend_from_slice(b"DIRC");
    push_u32(&mut data, 2);
    push_u32(&mut data, entries.len() as u32);
    for entry in entries {
        let entry_start = data.len();
        push_u32(&mut data, 0);
        push_u32(&mut data, 0);
        push_u32(&mut data, entry.mtime_sec);
        push_u32(&mut data, entry.mtime_nsec);
        push_u32(&mut data, 0);
        push_u32(&mut data, 0);
        push_u32(&mut data, entry.mode);
        push_u32(&mut data, 0);
        push_u32(&mut data, 0);
        push_u32(&mut data, entry.size);
        data.extend_from_slice(&entry.oid);
        push_u16(&mut data, entry.path.len().min(0x0fff) as u16);
        data.extend_from_slice(entry.path.as_bytes());
        data.push(0);
        while (data.len() - entry_start) % 8 != 0 {
            data.push(0);
        }
    }
    let checksum = sha1_bytes(&data);
    data.extend_from_slice(&checksum);
    let index_path = repo.git_dir.join("index");
    fs::write(&index_path, data).map_err(|error| format!("Failed to write index: {error}"))?;
    invalidate_index_cache(&repo.git_dir);
    Ok(())
}

fn invalidate_index_cache(git_dir: &Path) {
    if let Some(cache) = INDEX_CACHE.get() {
        cache.write().unwrap().remove(git_dir);
    }
}

fn index_entry_for_file(repo: &GitRepo, path: &Path) -> Result<IndexEntry, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let autocrlf = autocrlf_for(repo);
    let normalized = normalize_for_hash(&bytes, autocrlf);
    let oid = write_object(repo, "blob", &normalized)?;
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Failed to stat {}: {error}", path.display()))?;
    let (mtime_sec, mtime_nsec) = stat_mtime(&metadata);
    Ok(IndexEntry {
        path: to_repo_path(repo, path)?,
        oid,
        mode: 0o100644,
        size: metadata.len().min(u32::MAX as u64) as u32,
        mtime_sec,
        mtime_nsec,
    })
}

fn stat_mtime(metadata: &fs::Metadata) -> (u32, u32) {
    metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs() as u32, duration.subsec_nanos()))
        .unwrap_or((0, 0))
}

fn expand_add_path(repo: &GitRepo, path: &str) -> Result<Vec<PathBuf>, String> {
    let path = ensure_path_allowed(path)?;
    if path.is_dir() {
        return collect_files(&path, repo);
    }
    Ok(vec![path])
}

// 4. Status -------------------------------------------------------------------
fn status_text(repo: &GitRepo, include_untracked: bool) -> Result<String, String> {
    let branch = branch_name(repo).unwrap_or_else(|| "HEAD".to_string());
    let mut lines = vec![format!("## {branch}")];
    let index = read_index(repo)?;
    let index_map = index
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect::<HashMap<_, _>>();
    let head_map = head_tree_map(repo).unwrap_or_default();
    let tracked = index_map
        .keys()
        .chain(head_map.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let worktree = worktree_blob_map_for_paths(repo, tracked.iter(), &index_map)?;
    let untracked = if include_untracked {
        untracked_files(repo, &tracked)?
    } else {
        BTreeSet::new()
    };
    let mut paths = tracked;
    if include_untracked {
        paths.extend(untracked.iter().cloned());
    }

    for path in paths {
        let head_oid = head_map.get(&path);
        let index_entry = index_map.get(&path);
        let work_oid = worktree.get(&path);
        let is_untracked = untracked.contains(&path);
        let index_code = match (head_oid, index_entry) {
            (None, Some(_)) => "A",
            (Some(_), None) => "D",
            (Some(head), Some(index)) if head != &index.oid => "M",
            _ => " ",
        };
        let work_code = match (index_entry, work_oid) {
            (None, _) if is_untracked => "?",
            (None, Some(_)) => " ",
            (Some(_), None) => "D",
            (Some(index), Some(work)) if index.oid != *work => "M",
            _ => " ",
        };
        if index_code != " " || work_code != " " {
            if index_entry.is_none() && head_oid.is_none() && is_untracked {
                lines.push(format!("?? {path}"));
            } else {
                lines.push(format!("{index_code}{work_code} {path}"));
            }
        }
    }

    Ok(lines.join("\n"))
}

// 5. Diff ---------------------------------------------------------------------
fn diff_changes(repo: &GitRepo, args: &Value) -> Result<Vec<DiffChange>, String> {
    let path_filter = string_array(args, "paths").unwrap_or_default();
    let mut changes = if let (Some(source), Some(target)) = (
        args.get("source").and_then(Value::as_str),
        args.get("target").and_then(Value::as_str),
    ) {
        let left = tree_map_for_spec(repo, source)?;
        let right = tree_map_for_spec(repo, target)?;
        compare_oid_maps(repo, &left, &right)?
    } else if bool_field(args, "staged", false) {
        let left = head_tree_map(repo).unwrap_or_default();
        let right = read_index(repo)?
            .into_iter()
            .map(|entry| (entry.path.clone(), entry.oid))
            .collect::<HashMap<_, _>>();
        compare_oid_maps(repo, &left, &right)?
    } else if let Some(target) = args.get("target").and_then(Value::as_str) {
        let left = tree_map_for_spec(repo, target)?;
        let right = worktree_blob_map(repo)?;
        compare_oid_maps(repo, &left, &right)?
    } else {
        let entries = read_index(repo)?;
        let stat_cache: HashMap<String, IndexEntry> = entries
            .iter()
            .map(|entry| (entry.path.clone(), entry.clone()))
            .collect();
        let left = entries
            .into_iter()
            .map(|entry| (entry.path, entry.oid))
            .collect::<HashMap<_, _>>();
        let right = if bool_field(args, "includeUntracked", false) {
            worktree_blob_map(repo)?
        } else {
            worktree_blob_map_for_paths(repo, left.keys(), &stat_cache)?
        };
        compare_oid_maps(repo, &left, &right)?
    };

    if !path_filter.is_empty() {
        changes.retain(|change| path_filter.iter().any(|path| change.path.starts_with(path)));
    }

    Ok(changes)
}

fn compare_oid_maps(
    repo: &GitRepo,
    left: &HashMap<String, [u8; 20]>,
    right: &HashMap<String, [u8; 20]>,
) -> Result<Vec<DiffChange>, String> {
    let mut paths = BTreeSet::new();
    paths.extend(left.keys().cloned());
    paths.extend(right.keys().cloned());
    let mut changes = Vec::new();
    for path in paths {
        let old_oid = left.get(&path);
        let new_oid = right.get(&path);
        if old_oid == new_oid {
            continue;
        }
        let old = old_oid.and_then(|oid| read_blob(repo, oid).ok());
        let new = new_oid.and_then(|oid| read_blob(repo, oid).ok());
        changes.push(DiffChange { path, old, new });
    }

    Ok(changes)
}

fn diff_patch(changes: &[DiffChange]) -> String {
    let mut out = Vec::new();
    for change in changes {
        out.push(format!("diff --git a/{0} b/{0}", change.path));
        match (&change.old, &change.new) {
            (None, Some(new)) => {
                out.push("new file mode 100644".to_string());
                out.push("--- /dev/null".to_string());
                out.push(format!("+++ b/{}", change.path));
                let new_lines = lines_lossy(new);
                if !new_lines.is_empty() {
                    out.push(format!("@@ -0,0 +1,{} @@", new_lines.len()));
                    for line in &new_lines {
                        out.push(format!("+{line}"));
                    }
                }
            }
            (Some(old), None) => {
                out.push("deleted file mode 100644".to_string());
                out.push(format!("--- a/{}", change.path));
                out.push("+++ /dev/null".to_string());
                let old_lines = lines_lossy(old);
                if !old_lines.is_empty() {
                    out.push(format!("@@ -1,{} +0,0 @@", old_lines.len()));
                    for line in &old_lines {
                        out.push(format!("-{line}"));
                    }
                }
            }
            (Some(old), Some(new)) => {
                out.push(format!("--- a/{}", change.path));
                out.push(format!("+++ b/{}", change.path));
                let old_lines = lines_lossy(old);
                let new_lines = lines_lossy(new);
                if old_lines.len().saturating_mul(new_lines.len()) > DIFF_MYERS_MAX_PRODUCT {
                    push_full_file_hunk(&mut out, &old_lines, &new_lines);
                    continue;
                }
                let ops = myers_diff(&old_lines, &new_lines);
                for hunk in group_hunks(&ops, 3) {
                    out.push(format!(
                        "@@ -{},{} +{},{} @@",
                        hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
                    ));
                    for op in &hunk.ops {
                        match op {
                            DiffOp::Equal(old_index) => {
                                out.push(format!(" {}", old_lines[*old_index]));
                            }
                            DiffOp::Delete(old_index) => {
                                out.push(format!("-{}", old_lines[*old_index]));
                            }
                            DiffOp::Insert(new_index) => {
                                out.push(format!("+{}", new_lines[*new_index]));
                            }
                        }
                    }
                }
            }
            (None, None) => {}
        }
    }

    out.join("\n")
}

fn push_full_file_hunk(out: &mut Vec<String>, old_lines: &[String], new_lines: &[String]) {
    out.push(format!(
        "@@ -1,{} +1,{} @@",
        old_lines.len(),
        new_lines.len()
    ));
    for line in old_lines {
        out.push(format!("-{line}"));
    }
    for line in new_lines {
        out.push(format!("+{line}"));
    }
}

#[derive(Clone, Debug)]
enum DiffOp {
    Equal(usize),
    Delete(usize),
    Insert(usize),
}

struct Hunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    ops: Vec<DiffOp>,
}

fn myers_diff(old: &[String], new: &[String]) -> Vec<DiffOp> {
    let old_len = old.len();
    let new_len = new.len();
    if old_len == 0 && new_len == 0 {
        return Vec::new();
    }
    if old_len == 0 {
        return (0..new_len).map(DiffOp::Insert).collect();
    }
    if new_len == 0 {
        return (0..old_len).map(DiffOp::Delete).collect();
    }

    let max = old_len + new_len;
    let size = 2 * max + 1;
    let shift = max as isize;
    let idx = |k: isize| (k + shift) as usize;

    let mut v = vec![0isize; size];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut reached = false;
    'outer: for d in 0..=max as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[idx(k - 1)] < v[idx(k + 1)]) {
                v[idx(k + 1)]
            } else {
                v[idx(k - 1)] + 1
            };
            let mut y = x - k;
            while x < old_len as isize && y < new_len as isize && old[x as usize] == new[y as usize]
            {
                x += 1;
                y += 1;
            }
            v[idx(k)] = x;
            if x >= old_len as isize && y >= new_len as isize {
                reached = true;
                break 'outer;
            }
            k += 2;
        }
    }
    if !reached {
        trace.push(v.clone());
    }

    let mut ops = Vec::new();
    let mut x = old_len as isize;
    let mut y = new_len as isize;
    for d in (1..trace.len()).rev() {
        let snapshot = &trace[d];
        let k = x - y;
        let prev_k = if k == -(d as isize)
            || (k != d as isize && snapshot[idx(k - 1)] < snapshot[idx(k + 1)])
        {
            k + 1
        } else {
            k - 1
        };
        let prev_x = snapshot[idx(prev_k)];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(DiffOp::Equal((x - 1) as usize));
            x -= 1;
            y -= 1;
        }
        if x == prev_x {
            ops.push(DiffOp::Insert((y - 1) as usize));
            y -= 1;
        } else {
            ops.push(DiffOp::Delete((x - 1) as usize));
            x -= 1;
        }
    }
    while x > 0 && y > 0 {
        ops.push(DiffOp::Equal((x - 1) as usize));
        x -= 1;
        y -= 1;
    }
    ops.reverse();
    ops
}

fn group_hunks(ops: &[DiffOp], context: usize) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut cursor = 0usize;
    while cursor < ops.len() {
        let mut edit_start = cursor;
        while edit_start < ops.len() && matches!(ops[edit_start], DiffOp::Equal(_)) {
            edit_start += 1;
        }
        if edit_start == ops.len() {
            break;
        }
        let start = edit_start.saturating_sub(context).max(cursor);
        let mut last_edit = edit_start;
        let mut end = edit_start + 1;
        while end < ops.len() {
            if matches!(ops[end], DiffOp::Equal(_)) {
                if end - last_edit > 2 * context {
                    break;
                }
            } else {
                last_edit = end;
            }
            end += 1;
        }
        let real_end = (last_edit + 1 + context).min(ops.len());

        let mut old_index = 0usize;
        let mut new_index = 0usize;
        for op in &ops[..start] {
            match op {
                DiffOp::Equal(_) => {
                    old_index += 1;
                    new_index += 1;
                }
                DiffOp::Delete(_) => old_index += 1,
                DiffOp::Insert(_) => new_index += 1,
            }
        }
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        let mut hunk_ops = Vec::new();
        for op in &ops[start..real_end] {
            match op {
                DiffOp::Equal(_) => {
                    old_count += 1;
                    new_count += 1;
                }
                DiffOp::Delete(_) => old_count += 1,
                DiffOp::Insert(_) => new_count += 1,
            }
            hunk_ops.push(op.clone());
        }
        hunks.push(Hunk {
            old_start: if old_count == 0 {
                old_index
            } else {
                old_index + 1
            },
            old_count,
            new_start: if new_count == 0 {
                new_index
            } else {
                new_index + 1
            },
            new_count,
            ops: hunk_ops,
        });
        cursor = real_end;
    }
    hunks
}

fn diff_stat(changes: &[DiffChange]) -> String {
    let mut lines = Vec::new();
    let mut files = 0usize;
    for change in changes {
        files += 1;
        let old_lines = change
            .old
            .as_ref()
            .map(|value| lines_lossy(value).len())
            .unwrap_or(0);
        let new_lines = change
            .new
            .as_ref()
            .map(|value| lines_lossy(value).len())
            .unwrap_or(0);
        lines.push(format!("{} | -{} +{}", change.path, old_lines, new_lines));
    }
    lines.push(format!("{files} files changed"));
    lines.join("\n")
}

// 6. Show and commit ----------------------------------------------------------
fn show_object(repo: &GitRepo, spec: &str, file_path: Option<&str>) -> Result<String, String> {
    let (spec, file_path) = if file_path.is_none() {
        if let Some((object, path)) = spec.split_once(':') {
            (object, Some(path))
        } else {
            (spec, file_path)
        }
    } else {
        (spec, file_path)
    };
    let oid = resolve_oid(repo, spec)?;
    if let Some(file_path) = file_path {
        let tree = if let Some(tree_oid) = read_commit_tree(repo, &oid)? {
            tree_oid
        } else {
            oid
        };
        let files = tree_files(repo, &tree, "")?;
        let Some(blob_oid) = files.get(file_path) else {
            return Err(format!("File not found in tree: {file_path}"));
        };
        return read_blob(repo, blob_oid).map(|bytes| String::from_utf8_lossy(&bytes).to_string());
    }

    let object = read_object(repo, &oid)?;
    match object.kind.as_str() {
        "commit" => Ok(String::from_utf8_lossy(&object.data).to_string()),
        "blob" => Ok(String::from_utf8_lossy(&object.data).to_string()),
        "tree" => {
            let entries = parse_tree(&object.data)?;
            Ok(entries
                .iter()
                .map(|entry| format!("{} {} {}", entry.mode, oid_hex(&entry.oid), entry.name))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        kind => Ok(format!("{kind} {}", oid_hex(&oid))),
    }
}

fn write_commit(
    repo: &GitRepo,
    tree_oid: [u8; 20],
    parents: &[[u8; 20]],
    signature: &str,
    message: &str,
) -> Result<[u8; 20], String> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("tree {}\n", oid_hex(&tree_oid)).as_bytes());
    for parent in parents {
        body.extend_from_slice(format!("parent {}\n", oid_hex(parent)).as_bytes());
    }
    body.extend_from_slice(format!("author {signature}\n").as_bytes());
    body.extend_from_slice(format!("committer {signature}\n\n").as_bytes());
    body.extend_from_slice(message.as_bytes());
    if !message.ends_with('\n') {
        body.push(b'\n');
    }

    write_object(repo, "commit", &body)
}

fn commit_message(args: &Value) -> Result<String, String> {
    if let Some(path) = args.get("messagePath").and_then(Value::as_str) {
        let path = ensure_path_allowed(path)?;
        let offset = args
            .get("messageOffset")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let length = args
            .get("messageLength")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        return read_text_slice(path, offset, length);
    }

    args.get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "message or messagePath is required".to_string())
}

fn signature(args: &Value) -> String {
    let (name, email) = args
        .get("author")
        .and_then(Value::as_object)
        .map(|author| {
            (
                author
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("rust-fs-mcp")
                    .to_string(),
                author
                    .get("email")
                    .and_then(Value::as_str)
                    .unwrap_or("rust-fs-mcp@example.invalid")
                    .to_string(),
            )
        })
        .unwrap_or_else(|| {
            (
                std::env::var("GIT_AUTHOR_NAME").unwrap_or_else(|_| "rust-fs-mcp".to_string()),
                std::env::var("GIT_AUTHOR_EMAIL")
                    .unwrap_or_else(|_| "rust-fs-mcp@example.invalid".to_string()),
            )
        });
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("{name} <{email}> {seconds} +0000")
}

fn looks_conventional(message: &str) -> bool {
    let Some(header) = message.lines().next() else {
        return false;
    };
    let Some((kind, summary)) = header.split_once(": ") else {
        return false;
    };

    let valid_type = kind
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch == '-' || ch == '(' || ch == ')');
    valid_type && summary.chars().any(|ch| ch.is_ascii_alphabetic())
}

// 7. Tree and object storage --------------------------------------------------
fn write_tree(repo: &GitRepo, entries: &[IndexEntry]) -> Result<[u8; 20], String> {
    let mut root = TreeNode::default();
    for entry in entries {
        insert_tree_entry(&mut root, entry);
    }
    write_tree_node(repo, &root)
}

fn insert_tree_entry(node: &mut TreeNode, entry: &IndexEntry) {
    let mut parts = entry.path.split('/').collect::<Vec<_>>();
    let Some(file_name) = parts.pop() else {
        return;
    };
    let mut cursor = node;
    for part in parts {
        cursor = cursor.dirs.entry(part.to_string()).or_default();
    }
    cursor.files.insert(file_name.to_string(), entry.clone());
}

fn write_tree_node(repo: &GitRepo, node: &TreeNode) -> Result<[u8; 20], String> {
    let mut rows = Vec::new();
    for (name, child) in &node.dirs {
        let oid = write_tree_node(repo, child)?;
        rows.push(("40000".to_string(), name.clone(), oid));
    }
    for (name, entry) in &node.files {
        rows.push((format!("{:o}", entry.mode), name.clone(), entry.oid));
    }
    rows.sort_by(|left, right| left.1.cmp(&right.1));

    let mut data = Vec::new();
    for (mode, name, oid) in rows {
        data.extend_from_slice(format!("{mode} {name}").as_bytes());
        data.push(0);
        data.extend_from_slice(&oid);
    }
    write_object(repo, "tree", &data)
}

fn read_object(repo: &GitRepo, oid: &[u8; 20]) -> Result<Arc<GitObject>, String> {
    if let Some(object) = cached_object(oid) {
        return Ok(object);
    }
    if let Some(object) = read_loose_object(repo, oid)? {
        return Ok(cache_object(*oid, object));
    }
    if let Some(object) = read_packed_object(repo, oid)? {
        return Ok(cache_object(*oid, object));
    }
    Err(format!("Failed to read object {}", oid_hex(oid)))
}

fn cached_object(oid: &[u8; 20]) -> Option<Arc<GitObject>> {
    OBJECT_CACHE
        .get_or_init(|| Mutex::new(ObjectCache::default()))
        .lock()
        .unwrap()
        .get(oid)
}

fn cache_object(oid: [u8; 20], object: GitObject) -> Arc<GitObject> {
    let object = Arc::new(object);
    OBJECT_CACHE
        .get_or_init(|| Mutex::new(ObjectCache::default()))
        .lock()
        .unwrap()
        .insert(oid, object.clone());
    object
}

fn object_size(object: &GitObject) -> usize {
    object.kind.len().saturating_add(object.data.len())
}

#[cfg(test)]
fn clear_object_cache() {
    if let Some(cache) = OBJECT_CACHE.get() {
        *cache.lock().unwrap() = ObjectCache::default();
    }
}

fn read_loose_object(repo: &GitRepo, oid: &[u8; 20]) -> Result<Option<GitObject>, String> {
    let hex = oid_hex(oid);
    let object_path = repo
        .git_dir
        .join("objects")
        .join(&hex[0..2])
        .join(&hex[2..]);
    if !object_path.exists() {
        return Ok(None);
    }
    let compressed =
        fs::read(&object_path).map_err(|error| format!("Failed to read object {hex}: {error}"))?;
    let mut decoder = ZlibDecoder::new(&compressed[..]);
    let mut inflated = Vec::new();
    decoder
        .read_to_end(&mut inflated)
        .map_err(|error| format!("Failed to inflate object {hex}: {error}"))?;
    let Some(header_end) = inflated.iter().position(|byte| *byte == 0) else {
        return Err(format!("Invalid object header: {hex}"));
    };
    let header = String::from_utf8_lossy(&inflated[..header_end]);
    let kind = header
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("Invalid object header: {hex}"))?
        .to_string();
    Ok(Some(GitObject {
        kind,
        data: inflated[header_end + 1..].to_vec(),
    }))
}

fn write_object(repo: &GitRepo, kind: &str, data: &[u8]) -> Result<[u8; 20], String> {
    let mut full = Vec::new();
    full.extend_from_slice(format!("{kind} {}\0", data.len()).as_bytes());
    full.extend_from_slice(data);
    let oid = sha1_bytes(&full);
    let hex = oid_hex(&oid);
    let dir = repo.git_dir.join("objects").join(&hex[0..2]);
    let path = dir.join(&hex[2..]);
    if !path.exists() {
        ensure_object_dir(&dir)?;
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&full)
            .map_err(|error| format!("Failed to compress object: {error}"))?;
        let compressed = encoder
            .finish()
            .map_err(|error| format!("Failed to finish compression: {error}"))?;
        match fs::write(&path, &compressed) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir_all(&dir)
                    .map_err(|error| format!("Failed to create object directory: {error}"))?;
                if let Some(cache) = OBJECT_DIR_CACHE.get() {
                    cache.lock().unwrap().insert(dir.clone());
                }
                fs::write(&path, &compressed)
                    .map_err(|error| format!("Failed to write object {hex}: {error}"))?;
            }
            Err(error) => return Err(format!("Failed to write object {hex}: {error}")),
        }
    }
    cache_object(
        oid,
        GitObject {
            kind: kind.to_string(),
            data: data.to_vec(),
        },
    );
    Ok(oid)
}

fn ensure_object_dir(dir: &Path) -> Result<(), String> {
    let cache = OBJECT_DIR_CACHE.get_or_init(|| Mutex::new(HashSet::new()));
    {
        let dirs = cache.lock().unwrap();
        if dirs.contains(dir) {
            return Ok(());
        }
    }
    fs::create_dir_all(dir)
        .map_err(|error| format!("Failed to create object directory: {error}"))?;
    cache.lock().unwrap().insert(dir.to_path_buf());
    Ok(())
}

fn parse_tree(data: &[u8]) -> Result<Vec<TreeEntry>, String> {
    let mut offset = 0usize;
    let mut entries = Vec::new();
    while offset < data.len() {
        let mode_end = data[offset..]
            .iter()
            .position(|byte| *byte == b' ')
            .map(|position| offset + position)
            .ok_or_else(|| "Invalid tree mode".to_string())?;
        let name_start = mode_end + 1;
        let name_end = data[name_start..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|position| name_start + position)
            .ok_or_else(|| "Invalid tree name".to_string())?;
        if name_end + 21 > data.len() {
            return Err("Truncated tree object".to_string());
        }
        let mut oid = [0u8; 20];
        oid.copy_from_slice(&data[name_end + 1..name_end + 21]);
        entries.push(TreeEntry {
            mode: String::from_utf8_lossy(&data[offset..mode_end]).to_string(),
            name: String::from_utf8_lossy(&data[name_start..name_end]).to_string(),
            oid,
        });
        offset = name_end + 21;
    }

    Ok(entries)
}

fn tree_files(
    repo: &GitRepo,
    tree_oid: &[u8; 20],
    prefix: &str,
) -> Result<HashMap<String, [u8; 20]>, String> {
    let object = read_object(repo, tree_oid)?;
    if object.kind != "tree" {
        return Err("Object is not a tree".to_string());
    }
    let mut files = HashMap::new();
    for entry in parse_tree(&object.data)? {
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        if entry.mode == "40000" {
            files.extend(tree_files(repo, &entry.oid, &path)?);
        } else {
            files.insert(path, entry.oid);
        }
    }

    Ok(files)
}

// 8. Revision helpers ---------------------------------------------------------
fn resolve_oid(repo: &GitRepo, spec: &str) -> Result<[u8; 20], String> {
    if spec == "HEAD" {
        return read_head_oid(repo)?.ok_or_else(|| "HEAD is unborn".to_string());
    }
    if let Some(oid) = hex_to_oid(spec) {
        return Ok(oid);
    }
    if let Some(oid) = read_ref(repo, &format!("refs/heads/{spec}"))? {
        return Ok(oid);
    }
    if let Some(oid) = read_ref(repo, spec)? {
        return Ok(oid);
    }
    if spec.len() >= 4 && spec.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return find_object_prefix(repo, spec);
    }

    Err(format!("Unsupported revision: {spec}"))
}

fn read_head_oid(repo: &GitRepo) -> Result<Option<[u8; 20]>, String> {
    let head = fs::read_to_string(repo.git_dir.join("HEAD"))
        .map_err(|error| format!("Failed to read HEAD: {error}"))?;
    if let Some(reference) = head.trim().strip_prefix("ref: ") {
        return read_ref(repo, reference);
    }
    Ok(hex_to_oid(head.trim()))
}

fn update_head(repo: &GitRepo, oid: &[u8; 20]) -> Result<(), String> {
    let head = fs::read_to_string(repo.git_dir.join("HEAD"))
        .map_err(|error| format!("Failed to read HEAD: {error}"))?;
    if let Some(reference) = head.trim().strip_prefix("ref: ") {
        let path = repo.git_dir.join(reference);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Failed to create ref directory: {error}"))?;
        }
        fs::write(path, format!("{}\n", oid_hex(oid)))
            .map_err(|error| format!("Failed to update ref: {error}"))?;
    } else {
        fs::write(repo.git_dir.join("HEAD"), format!("{}\n", oid_hex(oid)))
            .map_err(|error| format!("Failed to update HEAD: {error}"))?;
    }
    Ok(())
}

fn read_ref(repo: &GitRepo, reference: &str) -> Result<Option<[u8; 20]>, String> {
    let path = repo.git_dir.join(reference);
    if path.exists() {
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        return Ok(hex_to_oid(text.trim()));
    }
    let packed = repo.git_dir.join("packed-refs");
    if packed.exists() {
        let text = fs::read_to_string(&packed)
            .map_err(|error| format!("Failed to read packed-refs: {error}"))?;
        for line in text.lines() {
            if line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let oid = parts.next();
            let name = parts.next();
            if name == Some(reference) {
                return Ok(oid.and_then(hex_to_oid));
            }
        }
    }

    Ok(None)
}

fn read_commit_tree(repo: &GitRepo, oid: &[u8; 20]) -> Result<Option<[u8; 20]>, String> {
    let object = read_object(repo, oid)?;
    if object.kind != "commit" {
        return Ok(None);
    }
    for line in String::from_utf8_lossy(&object.data).lines() {
        if let Some(tree) = line.strip_prefix("tree ") {
            return Ok(hex_to_oid(tree));
        }
    }
    Ok(None)
}

fn read_commit_parents(repo: &GitRepo, oid: &[u8; 20]) -> Result<Vec<[u8; 20]>, String> {
    let object = read_object(repo, oid)?;
    if object.kind != "commit" {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&object.data)
        .lines()
        .filter_map(|line| line.strip_prefix("parent ").and_then(hex_to_oid))
        .collect())
}

fn head_tree_map(repo: &GitRepo) -> Result<HashMap<String, [u8; 20]>, String> {
    let Some(head) = read_head_oid(repo)? else {
        return Ok(HashMap::new());
    };
    let Some(tree) = read_commit_tree(repo, &head)? else {
        return Ok(HashMap::new());
    };
    tree_files(repo, &tree, "")
}

fn tree_map_for_spec(repo: &GitRepo, spec: &str) -> Result<HashMap<String, [u8; 20]>, String> {
    let oid = resolve_oid(repo, spec)?;
    let tree = if let Some(tree) = read_commit_tree(repo, &oid)? {
        tree
    } else {
        oid
    };
    tree_files(repo, &tree, "")
}

fn branch_name(repo: &GitRepo) -> Option<String> {
    let head = fs::read_to_string(repo.git_dir.join("HEAD")).ok()?;
    if let Some(reference) = head.trim().strip_prefix("ref: refs/heads/") {
        return Some(reference.to_string());
    }
    Some(head.trim().chars().take(12).collect())
}

fn find_object_prefix(repo: &GitRepo, prefix: &str) -> Result<[u8; 20], String> {
    let mut matches: BTreeSet<[u8; 20]> = BTreeSet::new();
    let objects = repo.git_dir.join("objects");
    if let Ok(entries) = fs::read_dir(&objects) {
        for dir in entries {
            let Ok(dir) = dir else { continue };
            let dir_name = dir.file_name().to_string_lossy().to_string();
            if dir_name.len() != 2 || !prefix.starts_with(&dir_name) {
                continue;
            }
            let Ok(files) = fs::read_dir(dir.path()) else {
                continue;
            };
            for file in files {
                let Ok(file) = file else { continue };
                let candidate = format!("{}{}", dir_name, file.file_name().to_string_lossy());
                if !candidate.starts_with(prefix) {
                    continue;
                }
                if let Some(oid) = hex_to_oid(&candidate) {
                    matches.insert(oid);
                }
            }
        }
    }
    let packset = get_packset(&repo.git_dir)?;
    for pack in &packset.packs {
        for (oid, _) in &pack.entries {
            if oid_hex(oid).starts_with(prefix) {
                matches.insert(*oid);
            }
        }
    }
    match matches.len() {
        1 => Ok(*matches.iter().next().unwrap()),
        0 => Err(format!("Object not found: {prefix}")),
        _ => Err(format!("Ambiguous object prefix: {prefix}")),
    }
}

// 9.5 Packfile reader ---------------------------------------------------------
struct PackSet {
    packs: Vec<Pack>,
}

struct Pack {
    data: Vec<u8>,
    entries: Vec<([u8; 20], u64)>,
}

fn read_packed_object(repo: &GitRepo, oid: &[u8; 20]) -> Result<Option<GitObject>, String> {
    let packset = get_packset(&repo.git_dir)?;
    for pack in &packset.packs {
        if let Ok(index) = pack.entries.binary_search_by(|entry| entry.0.cmp(oid)) {
            let offset = pack.entries[index].1;
            let object = read_pack_object_at(pack, offset, repo)?;
            return Ok(Some(object));
        }
    }
    Ok(None)
}

fn get_packset(git_dir: &Path) -> Result<Arc<PackSet>, String> {
    let cache = PACK_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    {
        let map = cache.read().unwrap();
        if let Some(set) = map.get(git_dir) {
            return Ok(set.clone());
        }
    }
    let set = Arc::new(load_packset(git_dir)?);
    cache
        .write()
        .unwrap()
        .insert(git_dir.to_path_buf(), set.clone());
    Ok(set)
}

fn load_packset(git_dir: &Path) -> Result<PackSet, String> {
    let pack_dir = git_dir.join("objects").join("pack");
    if !pack_dir.exists() {
        return Ok(PackSet { packs: Vec::new() });
    }
    let mut packs = Vec::new();
    for entry in
        fs::read_dir(&pack_dir).map_err(|error| format!("Failed to list pack dir: {error}"))?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        let pack_path = path.with_extension("pack");
        if !pack_path.exists() {
            continue;
        }
        let idx_data = fs::read(&path)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        let entries = parse_pack_idx(&idx_data)?;
        let data = fs::read(&pack_path)
            .map_err(|error| format!("Failed to read {}: {error}", pack_path.display()))?;
        packs.push(Pack { data, entries });
    }
    Ok(PackSet { packs })
}

fn parse_pack_idx(data: &[u8]) -> Result<Vec<([u8; 20], u64)>, String> {
    if data.len() < 8 {
        return Err("Pack index too small".to_string());
    }
    if data[..4] != [0xff, 0x74, 0x4f, 0x63] {
        return Err("Only pack index v2 is supported".to_string());
    }
    let version = be_u32(&data[4..8]);
    if version != 2 {
        return Err(format!("Unsupported pack index version {version}"));
    }
    let fanout_end = 8 + 256 * 4;
    if data.len() < fanout_end {
        return Err("Pack index missing fan-out".to_string());
    }
    let total = be_u32(&data[fanout_end - 4..fanout_end]) as usize;
    let oid_start = fanout_end;
    let crc_start = oid_start + total * 20;
    let offset_start = crc_start + total * 4;
    let big_offsets_start = offset_start + total * 4;
    if data.len() < big_offsets_start {
        return Err("Pack index truncated".to_string());
    }
    let mut entries = Vec::with_capacity(total);
    for index in 0..total {
        let mut oid = [0u8; 20];
        oid.copy_from_slice(&data[oid_start + index * 20..oid_start + index * 20 + 20]);
        let off32 = be_u32(&data[offset_start + index * 4..offset_start + index * 4 + 4]);
        let offset = if off32 & 0x8000_0000 != 0 {
            let big_index = (off32 & 0x7fff_ffff) as usize;
            let start = big_offsets_start + big_index * 8;
            if start + 8 > data.len() {
                return Err("Pack index big-offset out of range".to_string());
            }
            be_u64(&data[start..start + 8])
        } else {
            off32 as u64
        };
        entries.push((oid, offset));
    }
    entries.sort_by_key(|entry| entry.0);
    Ok(entries)
}

fn read_pack_object_at(pack: &Pack, offset: u64, repo: &GitRepo) -> Result<GitObject, String> {
    let data = &pack.data;
    let mut cursor = offset as usize;
    if cursor >= data.len() {
        return Err("Pack offset out of range".to_string());
    }
    let mut byte = data[cursor];
    cursor += 1;
    let kind = (byte >> 4) & 0x07;
    let mut shift = 4u32;
    let mut _size = (byte & 0x0f) as u64;
    while byte & 0x80 != 0 {
        byte = data[cursor];
        cursor += 1;
        _size |= ((byte & 0x7f) as u64) << shift;
        shift += 7;
    }

    match kind {
        1..=4 => {
            let kind_str = match kind {
                1 => "commit",
                2 => "tree",
                3 => "blob",
                4 => "tag",
                _ => unreachable!(),
            }
            .to_string();
            let inflated = inflate_pack(&data[cursor..])?;
            Ok(GitObject {
                kind: kind_str,
                data: inflated,
            })
        }
        6 => {
            let (rel_offset, consumed) = read_ofs_delta_offset(&data[cursor..])?;
            cursor += consumed;
            if rel_offset > offset {
                return Err("Pack OFS_DELTA offset before pack start".to_string());
            }
            let base_offset = offset - rel_offset;
            let base = read_pack_object_at(pack, base_offset, repo)?;
            let delta = inflate_pack(&data[cursor..])?;
            let target = apply_pack_delta(&base.data, &delta)?;
            Ok(GitObject {
                kind: base.kind.clone(),
                data: target,
            })
        }
        7 => {
            if cursor + 20 > data.len() {
                return Err("Pack REF_DELTA truncated".to_string());
            }
            let mut base_oid = [0u8; 20];
            base_oid.copy_from_slice(&data[cursor..cursor + 20]);
            cursor += 20;
            let base = read_object(repo, &base_oid)?;
            let delta = inflate_pack(&data[cursor..])?;
            let target = apply_pack_delta(&base.data, &delta)?;
            Ok(GitObject {
                kind: base.kind.clone(),
                data: target,
            })
        }
        kind => Err(format!("Unsupported pack object kind {kind}")),
    }
}

fn inflate_pack(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = ZlibDecoder::new(input);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|error| format!("Failed to inflate pack data: {error}"))?;
    Ok(out)
}

fn read_ofs_delta_offset(data: &[u8]) -> Result<(u64, usize), String> {
    let mut cursor = 0usize;
    if cursor >= data.len() {
        return Err("OFS_DELTA truncated".to_string());
    }
    let mut byte = data[cursor];
    cursor += 1;
    let mut offset = (byte & 0x7f) as u64;
    while byte & 0x80 != 0 {
        offset += 1;
        offset <<= 7;
        if cursor >= data.len() {
            return Err("OFS_DELTA truncated".to_string());
        }
        byte = data[cursor];
        cursor += 1;
        offset |= (byte & 0x7f) as u64;
    }
    Ok((offset, cursor))
}

fn apply_pack_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, String> {
    let mut cursor = 0usize;
    let (_src_size, used) = read_delta_size(&delta[cursor..])?;
    cursor += used;
    let (tgt_size, used) = read_delta_size(&delta[cursor..])?;
    cursor += used;
    let mut out = Vec::with_capacity(tgt_size);
    while cursor < delta.len() {
        let cmd = delta[cursor];
        cursor += 1;
        if cmd & 0x80 != 0 {
            let mut copy_offset = 0u64;
            let mut copy_size = 0u64;
            for index in 0..4 {
                if cmd & (1 << index) != 0 {
                    if cursor >= delta.len() {
                        return Err("Delta copy truncated".to_string());
                    }
                    copy_offset |= (delta[cursor] as u64) << (index * 8);
                    cursor += 1;
                }
            }
            for index in 0..3 {
                if cmd & (1 << (4 + index)) != 0 {
                    if cursor >= delta.len() {
                        return Err("Delta copy truncated".to_string());
                    }
                    copy_size |= (delta[cursor] as u64) << (index * 8);
                    cursor += 1;
                }
            }
            if copy_size == 0 {
                copy_size = 0x10000;
            }
            let start = copy_offset as usize;
            let end = start + copy_size as usize;
            if end > base.len() {
                return Err("Delta copy out of range".to_string());
            }
            out.extend_from_slice(&base[start..end]);
        } else if cmd != 0 {
            let count = cmd as usize;
            if cursor + count > delta.len() {
                return Err("Delta insert truncated".to_string());
            }
            out.extend_from_slice(&delta[cursor..cursor + count]);
            cursor += count;
        } else {
            return Err("Reserved delta opcode 0".to_string());
        }
    }
    if out.len() != tgt_size {
        return Err(format!(
            "Delta size mismatch: expected {tgt_size}, got {}",
            out.len()
        ));
    }
    Ok(out)
}

fn read_delta_size(data: &[u8]) -> Result<(usize, usize), String> {
    let mut cursor = 0usize;
    let mut value = 0usize;
    let mut shift = 0u32;
    loop {
        if cursor >= data.len() {
            return Err("Delta size truncated".to_string());
        }
        let byte = data[cursor];
        cursor += 1;
        value |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Ok((value, cursor))
}

fn be_u64(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(buf)
}

// 10. File and byte helpers ---------------------------------------------------
struct IgnoreRules {
    rules: Vec<IgnoreRule>,
}

struct IgnoreRule {
    pattern: String,
    negated: bool,
    dir_only: bool,
}

impl IgnoreRules {
    fn load(repo: &GitRepo) -> Result<Self, String> {
        let path = repo.worktree.join(".gitignore");
        if !path.exists() {
            return Ok(Self { rules: Vec::new() });
        }

        let text = fs::read_to_string(&path)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        let rules = text
            .lines()
            .filter_map(IgnoreRule::parse)
            .collect::<Vec<_>>();
        Ok(Self { rules })
    }

    fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let mut ignored = false;
        for rule in &self.rules {
            if rule.matches(rel, name, is_dir) {
                ignored = !rule.negated;
            }
        }
        ignored
    }

    fn can_prune(&self, rel: &str) -> bool {
        !self
            .rules
            .iter()
            .any(|rule| rule.negated && rule.may_match_descendant(rel))
    }
}

impl IgnoreRule {
    fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }

        let (negated, pattern) = if let Some(pattern) = line.strip_prefix('!') {
            (true, pattern)
        } else {
            (false, line)
        };
        let pattern = pattern.replace('\\', "/");
        let dir_only = pattern.ends_with('/');
        let pattern = pattern.trim_matches('/').to_string();
        if pattern.is_empty() {
            return None;
        }

        Some(Self {
            pattern,
            negated,
            dir_only,
        })
    }

    fn matches(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        let rel = rel.trim_end_matches('/');
        if ignore_pattern_match(&self.pattern, rel, name) {
            return true;
        }
        if !self.dir_only {
            return false;
        }

        let prefix = format!("{}/", self.pattern.trim_start_matches("**/"));
        is_dir && ignore_pattern_match(&self.pattern, rel, name) || rel.starts_with(&prefix)
    }

    fn may_match_descendant(&self, rel: &str) -> bool {
        let rel = rel.trim_end_matches('/');
        let pattern = self.pattern.trim_start_matches("**/");
        pattern == rel || pattern.starts_with(&format!("{rel}/"))
    }
}

fn untracked_files(repo: &GitRepo, tracked: &BTreeSet<String>) -> Result<BTreeSet<String>, String> {
    let ignores = IgnoreRules::load(repo)?;
    collect_untracked_parallel(repo, tracked, &ignores)
}

fn collect_untracked_parallel(
    repo: &GitRepo,
    tracked: &BTreeSet<String>,
    ignores: &IgnoreRules,
) -> Result<BTreeSet<String>, String> {
    let mut roots = Vec::new();
    let entries = fs::read_dir(&repo.worktree)
        .map_err(|error| format!("Failed to list {}: {error}", repo.worktree.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path == repo.git_dir || path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            continue;
        }
        roots.push(path);
    }
    if roots.len() <= 1 {
        let mut files = BTreeSet::new();
        for path in roots {
            collect_untracked_path(&path, repo, tracked, ignores, &mut files)?;
        }
        return Ok(files);
    }

    let mut collected = Vec::with_capacity(roots.len());
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(roots.len());
        for path in roots {
            handles.push(scope.spawn(move || {
                let mut files = BTreeSet::new();
                collect_untracked_path(&path, repo, tracked, ignores, &mut files)?;
                Ok::<_, String>(files)
            }));
        }
        for handle in handles {
            collected.push(
                handle
                    .join()
                    .map_err(|_| "Failed to join untracked worker".to_string())?,
            );
        }
        Ok::<_, String>(())
    })?;

    let mut files = BTreeSet::new();
    for result in collected {
        files.extend(result?);
    }
    Ok(files)
}

fn collect_untracked_path(
    root: &Path,
    repo: &GitRepo,
    tracked: &BTreeSet<String>,
    ignores: &IgnoreRules,
    files: &mut BTreeSet<String>,
) -> Result<(), String> {
    if root.starts_with(&repo.git_dir) {
        return Ok(());
    }
    if root.is_file() {
        let rel = to_repo_path(repo, root)?;
        if !tracked.contains(&rel) {
            files.insert(rel);
        }
        return Ok(());
    }
    let entries = fs::read_dir(root)
        .map_err(|error| format!("Failed to list {}: {error}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path == repo.git_dir || path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            continue;
        }

        let rel = to_repo_path(repo, &path)?;
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        let is_dir = file_type.is_dir();
        if ignores.is_ignored(&rel, is_dir) {
            if is_dir && !ignores.can_prune(&rel) {
                collect_untracked_path(&path, repo, tracked, ignores, files)?;
            }
            continue;
        }
        if is_dir {
            collect_untracked_path(&path, repo, tracked, ignores, files)?;
        } else if file_type.is_file() && !tracked.contains(&rel) {
            files.insert(rel);
        }
    }

    Ok(())
}

fn ignore_pattern_match(pattern: &str, rel: &str, name: &str) -> bool {
    if pattern == rel || pattern == name {
        return true;
    }
    if let Some(rest) = pattern.strip_prefix("**/") {
        return rest == name
            || rel == rest
            || rel.starts_with(&format!("{rest}/"))
            || rel.ends_with(&format!("/{rest}"))
            || rel.contains(&format!("/{rest}/"))
            || wildcard_match(rest, rel)
            || wildcard_match(rest, name);
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

fn worktree_files(repo: &GitRepo) -> Result<Vec<PathBuf>, String> {
    collect_files(&repo.worktree, repo)
}

fn collect_files(root: &Path, repo: &GitRepo) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_files_inner(root, repo, &mut files)?;
    Ok(files)
}

fn collect_files_inner(
    root: &Path,
    repo: &GitRepo,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if root.starts_with(&repo.git_dir) {
        return Ok(());
    }
    if root.is_file() {
        files.push(root.to_path_buf());
        return Ok(());
    }
    let entries = fs::read_dir(root)
        .map_err(|error| format!("Failed to list {}: {error}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path == repo.git_dir || path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            continue;
        }
        collect_files_inner(&path, repo, files)?;
    }
    Ok(())
}

fn worktree_blob_map(repo: &GitRepo) -> Result<HashMap<String, [u8; 20]>, String> {
    let autocrlf = autocrlf_for(repo);
    let stat_cache: HashMap<String, IndexEntry> = read_index(repo)
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let mut map = HashMap::new();
    for path in worktree_files(repo)? {
        let repo_path = to_repo_path(repo, &path)?;
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("Failed to stat {}: {error}", path.display()))?;
        if let Some(entry) = stat_cache.get(&repo_path) {
            let (mtime_sec, _) = stat_mtime(&metadata);
            let size = metadata.len().min(u32::MAX as u64) as u32;
            if entry.mtime_sec != 0 && entry.size == size && entry.mtime_sec == mtime_sec {
                map.insert(repo_path, entry.oid);
                continue;
            }
        }
        map.insert(repo_path, worktree_blob_oid(&path, &metadata, autocrlf)?);
    }
    Ok(map)
}

fn worktree_blob_map_for_paths<'a, I>(
    repo: &GitRepo,
    paths: I,
    stat_cache: &HashMap<String, IndexEntry>,
) -> Result<HashMap<String, [u8; 20]>, String>
where
    I: IntoIterator<Item = &'a String>,
{
    let autocrlf = autocrlf_for(repo);
    let mut map = HashMap::new();
    for repo_path in paths {
        let path = repo.worktree.join(repo_path);
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            continue;
        }
        if let Some(entry) = stat_cache.get(repo_path) {
            let (mtime_sec, _) = stat_mtime(&metadata);
            let size = metadata.len().min(u32::MAX as u64) as u32;
            if entry.mtime_sec != 0 && entry.size == size && entry.mtime_sec == mtime_sec {
                map.insert(repo_path.clone(), entry.oid);
                continue;
            }
        }
        map.insert(
            repo_path.clone(),
            worktree_blob_oid(&path, &metadata, autocrlf)?,
        );
    }
    Ok(map)
}

fn autocrlf_for(repo: &GitRepo) -> bool {
    let cache = AUTOCRLF_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    {
        let map = cache.read().unwrap();
        if let Some(value) = map.get(&repo.git_dir) {
            return *value;
        }
    }
    let value = detect_autocrlf(&repo.git_dir);
    cache.write().unwrap().insert(repo.git_dir.clone(), value);
    value
}

fn detect_autocrlf(git_dir: &Path) -> bool {
    let path = git_dir.join("config");
    let Ok(text) = fs::read_to_string(&path) else {
        return cfg!(windows);
    };
    let mut in_core = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(section) = trimmed.strip_prefix('[') {
            in_core = section
                .trim_end_matches(']')
                .trim()
                .eq_ignore_ascii_case("core");
        } else if in_core
            && let Some((key, value)) = trimmed.split_once('=')
            && key.trim().eq_ignore_ascii_case("autocrlf")
        {
            let value = value.trim();
            return value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("input");
        }
    }
    cfg!(windows)
}

fn normalize_for_hash(bytes: &[u8], autocrlf: bool) -> Vec<u8> {
    if !autocrlf || bytes.contains(&0) {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' && index + 1 < bytes.len() && bytes[index + 1] == b'\n' {
            out.push(b'\n');
            index += 2;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    out
}

fn worktree_blob_oid(
    path: &Path,
    metadata: &fs::Metadata,
    autocrlf: bool,
) -> Result<[u8; 20], String> {
    if !autocrlf {
        return object_oid_for_file(path, metadata.len());
    }
    let bytes =
        fs::read(path).map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let normalized = normalize_for_hash(&bytes, autocrlf);
    Ok(object_oid("blob", &normalized))
}

fn object_oid_for_file(path: &Path, size: u64) -> Result<[u8; 20], String> {
    let file = fs::File::open(path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut buffer = [0u8; 64 * 1024];
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {size}\0").as_bytes());
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest);
    Ok(out)
}

fn read_blob(repo: &GitRepo, oid: &[u8; 20]) -> Result<Vec<u8>, String> {
    let object = read_object(repo, oid)?;
    if object.kind != "blob" {
        return Err(format!("Object is not a blob: {}", oid_hex(oid)));
    }
    Ok(object.data.clone())
}

fn to_repo_path(repo: &GitRepo, path: &Path) -> Result<String, String> {
    let path = ensure_path_allowed(path)?;
    let relative = path
        .strip_prefix(&repo.worktree)
        .map_err(|_| format!("Path is outside repository: {}", path.display()))?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn lines_lossy(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect()
}

fn object_oid(kind: &str, data: &[u8]) -> [u8; 20] {
    let mut full = Vec::new();
    full.extend_from_slice(format!("{kind} {}\0", data.len()).as_bytes());
    full.extend_from_slice(data);
    sha1_bytes(&full)
}

fn sha1_bytes(data: &[u8]) -> [u8; 20] {
    let digest = Sha1::digest(data);
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest);
    out
}

fn oid_hex(oid: &[u8; 20]) -> String {
    oid.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_to_oid(value: &str) -> Option<[u8; 20]> {
    if value.len() != 40 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let mut oid = [0u8; 20];
    for index in 0..20 {
        oid[index] = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(oid)
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn push_u32(data: &mut Vec<u8>, value: u32) {
    data.extend_from_slice(&value.to_be_bytes());
}

fn push_u16(data: &mut Vec<u8>, value: u16) {
    data.extend_from_slice(&value.to_be_bytes());
}

fn string_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

fn bool_field(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn first_line(value: &str) -> &str {
    value.lines().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_conventional_header() {
        assert!(looks_conventional("feat: add rust scaffold"));
        assert!(!looks_conventional("update"));
    }

    #[test]
    fn myers_diff_produces_minimal_edit() {
        let old: Vec<String> = ["a", "b", "c", "d", "e"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let new: Vec<String> = ["a", "x", "c", "d", "y", "e"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let ops = myers_diff(&old, &new);
        let mut summary = Vec::new();
        for op in &ops {
            match op {
                DiffOp::Equal(o) => summary.push(format!("={o}")),
                DiffOp::Delete(o) => summary.push(format!("-{o}")),
                DiffOp::Insert(n) => summary.push(format!("+{n}")),
            }
        }
        assert_eq!(summary, vec!["=0", "-1", "+1", "=2", "=3", "+4", "=4"]);
    }

    #[test]
    fn worktree_blob_map_reuses_oid_from_stat_cache() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-stat-cache-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir_all(&root).unwrap();
        init_repo(&root).unwrap();
        let file = root.join("a.txt");
        fs::write(&file, "hello\n").unwrap();
        let repo = discover_repo(&root).unwrap();
        let metadata = fs::metadata(&file).unwrap();
        let (mtime_sec, mtime_nsec) = stat_mtime(&metadata);
        let fake_oid = [9u8; 20];
        let entry = IndexEntry {
            path: "a.txt".to_string(),
            oid: fake_oid,
            mode: 0o100644,
            size: metadata.len() as u32,
            mtime_sec,
            mtime_nsec,
        };
        let mut cache = HashMap::new();
        cache.insert("a.txt".to_string(), entry);
        let paths = ["a.txt".to_string()];
        let map = worktree_blob_map_for_paths(&repo, paths.iter(), &cache).unwrap();
        assert_eq!(map.get("a.txt"), Some(&fake_oid), "stat cache should hit");

        let empty_cache = HashMap::new();
        let map2 = worktree_blob_map_for_paths(&repo, paths.iter(), &empty_cache).unwrap();
        assert_ne!(map2.get("a.txt"), Some(&fake_oid), "no cache should hash");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn diff_patch_emits_unified_hunks() {
        let change = DiffChange {
            path: "demo.txt".to_string(),
            old: Some(b"alpha\nbeta\ngamma\n".to_vec()),
            new: Some(b"alpha\nBETA\ngamma\ndelta\n".to_vec()),
        };
        let output = diff_patch(&[change]);
        assert!(output.contains("@@ -1,3 +1,4 @@"), "{output}");
        assert!(output.contains("-beta"), "{output}");
        assert!(output.contains("+BETA"), "{output}");
        assert!(output.contains("+delta"), "{output}");
    }

    #[test]
    fn diff_patch_falls_back_for_large_inputs() {
        let old = (0..2400)
            .map(|index| format!("old {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..2400)
            .map(|index| format!("new {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let change = DiffChange {
            path: "large.txt".to_string(),
            old: Some(old.into_bytes()),
            new: Some(new.into_bytes()),
        };

        let output = diff_patch(&[change]);
        assert!(output.contains("@@ -1,2400 +1,2400 @@"), "{output}");
        assert!(output.contains("-old 0"), "{output}");
        assert!(output.contains("+new 0"), "{output}");
    }

    #[test]
    fn hashes_blob_like_git() {
        assert_eq!(
            oid_hex(&object_oid("blob", b"hello\n")),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn hashes_file_blob_like_in_memory_blob() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-file-hash-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("blob.txt");
        fs::write(&file, b"streamed\nblob\n").unwrap();
        let metadata = fs::metadata(&file).unwrap();

        assert_eq!(
            object_oid_for_file(&file, metadata.len()).unwrap(),
            object_oid("blob", b"streamed\nblob\n")
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_object_reuses_cached_object() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-object-cache-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir_all(&root).unwrap();
        init_repo(&root).unwrap();
        let repo = discover_repo(&root).unwrap();
        let oid = write_object(&repo, "blob", b"cached\n").unwrap();
        clear_object_cache();

        let first = read_object(&repo, &oid).unwrap();
        assert_eq!(first.data, b"cached\n");

        let hex = oid_hex(&oid);
        let object_path = repo
            .git_dir
            .join("objects")
            .join(&hex[0..2])
            .join(&hex[2..]);
        fs::remove_file(object_path).unwrap();

        let second = read_object(&repo, &oid).unwrap();
        assert_eq!(second.data, b"cached\n");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn status_honors_gitignore_for_untracked_files() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-git-ignore-test-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir_all(&root).unwrap();
        init_repo(&root).unwrap();
        fs::write(
            root.join(".gitignore"),
            "**/target/\n**/vendor/\n!/vendor/\n!/vendor/tools/\n!/vendor/tools/**\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target").join("skip.txt"), "skip\n").unwrap();
        fs::create_dir_all(root.join("vendor").join("tools")).unwrap();
        fs::write(root.join("vendor").join("tools").join("keep.txt"), "keep\n").unwrap();

        let status = handle_git_status(&json!({
            "path": root.display().to_string(),
            "includeUntracked": true
        }));
        assert!(!status.is_error, "{status:?}");
        let text = status.structured.unwrap()["status"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!text.contains("?? target/skip.txt"), "{text}");
        assert!(text.contains("?? .gitignore"), "{text}");
        assert!(text.contains("?? vendor/tools/keep.txt"), "{text}");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn creates_commit_without_git_cli() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "rust-fs-mcp-git-test-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir_all(&root).unwrap();
        crate::core::config::handle_set_config_values(&json!({
            "items": [{
                "key": "allowedDirectories",
                "value": [std::env::current_dir().unwrap().display().to_string()]
            }]
        }));

        let cwd = handle_git_cwd(&json!({
            "path": root.display().to_string(),
            "initializeIfNotPresent": true
        }));
        assert!(!cwd.is_error, "{cwd:?}");

        fs::write(root.join("hello.txt"), "hello\n").unwrap();
        let add = handle_git_add(&json!({
            "path": root.join("hello.txt").display().to_string()
        }));
        assert!(!add.is_error, "{add:?}");

        let commit = handle_git_commit(&json!({
            "path": root.display().to_string(),
            "message": "feat: add hello\n\n- create fixture",
            "author": {
                "name": "rust-fs-mcp",
                "email": "rust-fs-mcp@example.invalid"
            }
        }));
        assert!(!commit.is_error, "{commit:?}");

        let show = handle_git_show(&json!({
            "path": root.display().to_string(),
            "object": "HEAD",
            "filePath": "hello.txt"
        }));
        assert!(!show.is_error, "{show:?}");

        fs::remove_dir_all(&root).unwrap();
    }
}
