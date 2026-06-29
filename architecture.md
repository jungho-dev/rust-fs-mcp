# Architecture

This document describes the current rust-fs-mcp architecture as implemented in the source tree.

## Goals

rust-fs-mcp is designed around four constraints:

- Keep public fs-mcp tool names and request shapes stable.
- Keep all tool results inside a normalized response envelope.
- Resolve external CLI tools (rg, fd, git) from PATH instead of bundling them with the binary, so the release artifact stays small and reuses the user's installed toolchain.
- Keep state explicit and process-local for configuration, search sessions, and git cwd.

## High-Level Flow

```text
MCP client
  |
  | line JSON-RPC over stdin/stdout
  v
src/main.rs
  |
  v
protocol::server::run
  |
  +-- initialize ------------------> server metadata and capabilities
  +-- tools/list ------------------> protocol::catalog::tool_catalog
  +-- tools/call ------------------> tools::dispatch_tool_call
  +-- resources/list --------------> empty list
  +-- resources/templates/list ----> empty list
```

A tools/call request enters dispatch_tool_call with the tool name and optional JSON arguments. The dispatcher resolves
args_path references first, calls the matching tool handler, then normalizes the RawResult into the public response
envelope.

## Request Lifecycle

1. src/main.rs calls rust_fs_mcp::server::run.
2. protocol::server reads stdin line by line.
3. Each non-empty line is parsed as JSON-RPC.
4. Notification methods return no response.
5. initialize returns protocol version, capabilities, server info, and server instructions; clients whose clientInfo.name contains claude receive gate-style routing instructions (built-in first, rust-fs-mcp for batch/precision work), all other clients receive the batch-first instructions.
6. tools/list returns catalog entries and input schemas.
7. tools/call extracts params.name and params.arguments.
8. tools::dispatch_tool_call resolves args_path, args_offset, and args_length.
9. The concrete tool handler returns RawResult.
10. core::response::normalize_tool_result builds the MCP content, structuredContent, _meta, and isError fields.

## Module Responsibilities

| Module | Role |
| --- | --- |
| main | Binary entry point and fatal error handling. |
| lib | Re-exports core, protocol, and tools modules. |
| protocol::server | JSON-RPC line protocol, method routing, initialize response, empty resource handlers. |
| protocol::catalog | Public tool registry, tool descriptions, annotations, and JSON schemas. |
| core::args_ref | Large argument indirection through args_path and optional character slicing. |
| core::batch | Shared batch execution and structured batch result format. |
| core::external | Spawns external CLI tools (rg, fd, git) resolved from PATH and captures stdout/stderr with timeouts. |
| core::config | RuntimeConfig (allowedDirectories), home expansion, lexical path normalization, and path-allowed checking with an internal cache. |
| core::response | RawResult type, content sanitization, display text, response timing, public envelope normalization. |
| tools::mod | Tool name dispatcher and cross-tool argument resolution boundary. |
| tools::fs_tools | File, directory, metadata, exact block edit (file-edit), 1-based line edit (file-edit-lines), image, and HTTP read behavior. |
| tools::search_tools | Content regex search execution backed by ripgrep resolved from PATH. |
| tools::inspect_tools | Compact read-only filesystem inspection collection for coding tasks. |
| tools::git_tools | Repository discovery plus git status/add/commit/diff/show by invoking the git CLI resolved from PATH. |
| tests::tool_matrix | End-to-end catalog and dispatch coverage for the public tool surface. |

## State Model

The server stores runtime state in process memory.

| State | Owner | Backing type | Lifetime |
| --- | --- | --- | --- |
| Runtime config | core::config | OnceLock<RwLock<ConfigState>> with a separate Mutex<HashMap<PathBuf, bool>> path-allowed cache | Process lifetime |
| Git cwd | tools::git_tools | OnceLock<Mutex<Option<PathBuf>>> | Until changed or process exit |

No state is persisted by the server except filesystem and git writes requested by tool calls.

## Configuration Boundary

core::config is the shared path and process-safety boundary.

Path handling:

- ~ is expanded from USERPROFILE or HOME.
- Relative paths are joined to the process current directory.
- Lexical components are normalized.
- allowedDirectories is checked after path resolution.
- If allowedDirectories is empty, local path access is unrestricted.
- target_path also checks the parent directory boundary.
- RUST_FS_MCP_TOOL_PROFILE=fast-coding limits tools/list to fs-inspect while dispatch compatibility remains available.
- RUST_FS_MCP_ALWAYS_LOAD (default file-read,fs-search,file-edit-lines) marks the listed tools with _meta {"anthropic/alwaysLoad": true} so schema-deferring hosts expose them upfront.

## Tool Dispatch Boundary

tools::dispatch_tool_call is the only public tool execution entry from the protocol layer.

It performs three steps:

1. Capture the start time.
2. Resolve top-level args_path references with inline override support.
3. Route the resolved Value to the named handler and normalize the result.

Unknown tool names return an error RawResult. They still use the normal response envelope.

## Response Contract

Tool handlers return RawResult. RawResult is intentionally small:

| Field | Purpose |
| --- | --- |
| content | MCP content blocks before normalization. |
| structured | Optional tool-specific structured data. |
| is_error | Tool-level error flag. |
| meta | Reserved per-tool metadata map. |

normalize_tool_result then produces the public contract:

- content: display-oriented text block.
- structuredContent.data.content: sanitized content blocks; result bodies (file contents, search lines, diffs, listings) are carried here exactly once.
- structuredContent.data.structuredContent: sanitized structured metadata or null; it never duplicates the body.
- structuredContent.durationMs: tool duration.
- structuredContent.error: message object, present only on failure.
- With the compact envelope disabled (RUST_FS_MCP_COMPACT=0) the response additionally carries data.text, error: null, schemaVersion: 1, status, and toolName.
- _meta.fsMcpResult: compact status metadata.
- isError: present only when the result is an error.

Text and JSON strings are sanitized before leaving the server.

## Batch Contract

Batch tools use run_batch and create_batch_response. The default compact batch layer preserves:

- 1-based input index.
- Per-item ok flag.
- Per-item data carrying the tool-specific structured metadata (omitted when null).
- failedCount, succeededCount, totalCount, and toolName.

With the compact envelope disabled each entry restores the verbatim input object and the
per-item result wrapper with content, structuredContent, and isError.

A batch response is marked as a tool error only when every item fails.

## Filesystem Architecture

fs_tools is split into read, write/directory, copy/move/remove/info/edit, shared helpers, and HTTP helpers.

Important contracts:

- Local paths pass through ensure_path_allowed, existing_path, or target_path.
- Writes create parent directories when needed.
- file-edit performs exact string replacement and can enforce expected_replacements.
- file-edit-lines replaces inclusive 1-based line ranges, preserving the file's original line endings.
- Binary files are detected through NUL bytes.
- Image files are returned as image content blocks with base64 data.
- Directory traversal honors depth, maxEntries, includeFiles, excludePatterns, and allowMissing.
- URL reads support http:// with redirect handling and reject https:// until TLS support exists.
- file-read-line-range reads local text ranges through native Rust streaming with a 1-based start line.
- dir-list uses native Rust traversal and falls back to fd from PATH when excludePatterns are supplied.

## Search Architecture

fs-search runs a single ripgrep-compatible content search per item and returns the batch result directly.

Backend:

- Content search shells out to ripgrep (rg) resolved from PATH.
- The resolver invokes commands by name through std::process::Command, so rg must be installed and available on PATH.
- Result structured data records the backend label (for example `path-rg`).

Search behavior:

- The search reads text-like files and matches with ripgrep regular expressions.
- ignoreCase sets case-insensitive matching.
- contextLines emits grep-like context separators.
- includeHidden controls dot-path traversal.
- filePattern uses glob matching to narrow the target files.
- maxResults caps the returned match lines.
- pattern_path can supply large patterns by reference.

## Git Architecture

git_tools wraps the git CLI resolved from PATH through core::external. Every handler builds an argument vector and runs it
with run_git inside the resolved worktree.

Repository discovery:

- path argument wins when provided; a file path uses its parent directory.
- Otherwise the session git-set-workdir value is used.
- The worktree root is confirmed with rev-parse --show-toplevel.

Command behavior:

- git-set-workdir resolves the worktree, can run git init first, and stores the Git working dir for later calls.
- git-add runs git add for the given paths, and adds --all, --update, or --force when those flags are set (all/update allow staging without an explicit pathspec).
- git-commit always injects -c user.name=rust-fs-mcp and -c user.email=rust-fs-mcp@example.invalid so commits succeed without local git config, adds --author when an author object is given, and forwards amend, allow-empty, and no-verify.
- git-amend rewrites HEAD: it requires an existing commit, reuses the message with --no-edit when none is given, otherwise validates a new Conventional Commit message, rejects combining author with reset-author, and forwards staged files, allow-empty, and no-verify.
- git-status runs git status --porcelain --branch.
- git-diff runs git diff with optional staged, name-only, stat, source/target, contextLines (mapped to --unified=<n>), and path arguments.
- git-show runs git show over the object or objects[] revision set with optional filePath pairing, stat (diffstat), and format=raw; multiple revisions resolve in one call.

Validation:

- Commit messages must pass an English Conventional Commit header check before git runs.
- run_git returns stdout on exit 0 and an stderr-based error otherwise.

Known git boundaries:

- A git binary must be installed and resolvable on PATH; there is no in-process git object store.
- Behavior and edge cases follow the installed git version, including submodule and rename-detection defaults.

## Inspect Architecture

inspect_tools exposes one composite tool, fs-inspect, for read-only coding lookups. A single call carries a root and a
list of requests, and each request is dispatched by its op.

Request ops:

- count-files counts glob matches under a directory with optional recursion and sample paths.
- search runs a regex or literal scan with optional capture-group field extraction and a file pattern filter.
- json-pick parses a JSON file and returns values at the requested JSON pointers.
- snippet returns context-bounded line ranges around substring matches.
- git-status delegates to git_tools::handle_git_status so filesystem and git state resolve in one round-trip.

Shared behavior:

- A per-call maxSnippetChars budget (default 6000) bounds total evidence text and sets the truncated flag when exceeded.
- Each answer carries id, op, status, value, confidence, evidence, and warnings; the call also returns scannedFiles, bytesRead, snippetChars, and truncated metrics.
- Compiled wildcard patterns are cached in a process-wide map, mirroring the search cache.
- RUST_FS_MCP_TOOL_PROFILE=fast-coding narrows tools/list to fs-inspect only.

## Tool Catalog Architecture

protocol::catalog is source of truth for exposed tool names, descriptions, schemas, and annotations. The dispatcher in
tools::mod must stay aligned with the catalog.

The tool matrix test enforces this alignment by:

1. Calling tools/list and collecting catalog names.
2. Calling every public tool through dispatch_tool_call.
3. Sorting and deduplicating the called names.
4. Comparing called names against the catalog.

Any new public tool must update catalog, dispatcher, and tests together.

## Validation Strategy

Use the narrowest command that covers the changed surface.

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build
```

For documentation-only changes, also inspect Markdown heading hierarchy, fenced code block languages, local links, and
trailing whitespace.
