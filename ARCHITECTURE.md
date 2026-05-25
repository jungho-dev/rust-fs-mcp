# Architecture

This document describes the current rust-fs-mcp architecture as implemented in the source tree.

## Goals

rust-fs-mcp is designed around four constraints:

- Keep public fs-mcp tool names and request shapes stable.
- Keep all tool results inside a normalized response envelope.
- Keep project-owned Rust tool executables in the Cargo build output for search and exclude-aware listing.
- Keep state explicit and process-local for configuration, search sessions, git cwd, and process sessions.

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
5. initialize returns protocol version, capabilities, server info, and server instructions.
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
| core::bundled | Resolves and runs project-bundled rg.exe, fd.exe, and bat.exe with timeouts. |
| core::config | Runtime configuration, path resolution, allowedDirectories enforcement, blocked command lookup. |
| core::response | RawResult type, content sanitization, display text, response timing, public envelope normalization. |
| tools::mod | Tool name dispatcher and cross-tool argument resolution boundary. |
| tools::fs_tools | File, directory, metadata, edit, image, and HTTP read behavior. |
| tools::search_tools | Search execution, in-memory search sessions, pagination, regex and literal matching. |
| tools::inspect_tools | Compact read-only filesystem inspection collection for coding tasks. |
| tools::git_tools | Repository discovery, git status/add/commit/diff/show without git CLI for supported paths. |
| tools::process_tools | Internal managed process helpers. No process tool is exposed in the public catalog. |
| tests::tool_matrix | End-to-end catalog and dispatch coverage for the public tool surface. |

## State Model

The server stores runtime state in process memory.

| State | Owner | Backing type | Lifetime |
| --- | --- | --- | --- |
| Runtime config | core::config | OnceLock<Mutex<RuntimeConfig>> | Process lifetime |
| Search sessions | tools::search_tools | OnceLock<Mutex<HashMap<String, SearchSession>>> | Until search-stop or process exit |
| Git cwd | tools::git_tools | OnceLock<Mutex<Option<PathBuf>>> | Until changed or process exit |
| Process sessions | tools::process_tools | OnceLock<Mutex<HashMap<i64, ProcSession>>> | Until process exit, kill, or server exit |

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

Internal process command handling:

- Lifecycle helpers validate the first command token against blockedCommands.
- cwd, command_path, and input_path are resolved through allowedDirectories when those helpers are used internally.
- These lifecycle helpers are not exported as public MCP tools.

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
- structuredContent.data.content: sanitized content blocks.
- structuredContent.data.structuredContent: sanitized structured data or null.
- structuredContent.durationMs: tool duration.
- structuredContent.error: null or message object.
- structuredContent.schemaVersion: 1.
- structuredContent.status: success or error.
- structuredContent.toolName: original tool name.
- _meta.fsMcpResult: compact status metadata.
- isError: present only when the result is an error.

Text and JSON strings are sanitized before leaving the server.

## Batch Contract

Batch tools use run_batch and create_batch_response. The batch layer preserves:

- 1-based input index.
- Original input object.
- Per-item ok flag.
- Per-item RawResult content, structuredContent, and isError.
- failedCount, succeededCount, totalCount, and toolName.

A batch response is marked as a tool error only when every item fails.

## Filesystem Architecture

fs_tools is split into read, write/directory, copy/move/remove/info/edit, shared helpers, and HTTP helpers.

Important contracts:

- Local paths pass through ensure_path_allowed, existing_path, or target_path.
- Writes create parent directories when needed.
- file-edit performs exact string replacement and can enforce expected_replacements.
- Binary files are detected through NUL bytes.
- Image files are returned as image content blocks with base64 data.
- Directory traversal honors depth, maxEntries, includeFiles, excludePatterns, and allowMissing.
- URL reads support http:// with redirect handling and reject https:// until TLS support exists.
- file-lines reads line ranges through the bundled bat.exe copied into the build output.
- dir-list uses the bundled fd.exe copied into the build output.

## Search Architecture

search_tools separates session mode from backend mode.

search-start and search-regex run the same search engine. search-start stores the result lines under a generated session id,
while search-regex returns the batch result directly. search-get pages stored lines by offset and length. search-stop removes
session ids.

Backend selection:

- content search runs the project-bundled rg.exe.
- files search runs the project-bundled fd.exe.
- The resolver never depends on PATH; it searches target/<profile>/tools and vendor/tools fallback locations.
- Result structured data includes the selected backend.

Search behavior:

- searchType files returns file paths only.
- searchType content reads text-like files and applies RegexBuilder.
- literalSearch escapes the pattern before compiling.
- ignoreCase sets case-insensitive matching.
- contextLines emits grep-like context separators.
- includeHidden controls dot-path traversal.
- filePattern uses wildcard matching against file name or displayed path.
- maxResults stops traversal early.

## Git Architecture

git_tools implements a compact git backend for the supported command surface.

Repository discovery:

- path argument wins when provided.
- Otherwise the pinned git-cwd is used.
- Otherwise the current directory is used.
- Discovery walks upward until .git is found.
- A .git file with gitdir: is supported.

Object and index handling:

- Blob, tree, and commit objects are written as loose zlib-compressed objects.
- Object ids use SHA-1 over the git object header plus data.
- The index reader and writer support git index version 2.
- References are read from loose refs or packed-refs.
- Object prefix resolution searches loose objects.

Command behavior:

- git-cwd can initialize a basic repository when requested.
- git-add writes blobs and updates the index.
- git-commit writes the tree and commit, then updates HEAD.
- git-status compares HEAD, index, and worktree maps.
- git-diff compares index/worktree, HEAD/index, target/worktree, or source/target maps.
- git-show renders commit, tree, blob, or revision:path content.

Known git boundaries:

- Packfile object storage is not implemented.
- Submodule handling is not implemented.
- Rename-aware diff is not implemented.
- Diff output is simple whole-file patch/stat generation, not a full git diff algorithm.

## Process Architecture

No process operation is exported as a public MCP tool in this catalog. process_tools can keep internal lifecycle helpers
for runtime integration, but process controls are not exposed through tools/list or dispatch_tool_call.

Internal start flow:

1. Read command or command_path.
2. Validate the first token against blockedCommands.
3. Resolve optional cwd through allowedDirectories.
4. Build shell invocation arguments.
5. Spawn the child with piped stdin, stdout, and stderr.
6. Start reader threads for stdout and stderr.
7. Store the ProcSession by pid.
8. Wait briefly for initial output according to timeout_ms.

Session behavior:

- stdout and stderr are appended to one shared output buffer.
- stderr lines are prefixed with stderr:.
- Input interaction, lifecycle listing, output read, and kill behavior are internal details unless a future public
  catalog intentionally adds them.

The server rejects unmanaged PIDs because it does not own their stdin, stdout, stderr, or lifecycle.

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
