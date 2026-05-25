# rust-fs-mcp

rust-fs-mcp is a Rust stdio MCP server that ports the public fs-mcp tool contracts to a native Rust
implementation. It exposes filesystem, search, and git tools through
line-oriented JSON-RPC over stdin/stdout.

The project keeps the Node fs-mcp contract shape where it matters: public tool names, batch-first inputs,
large-argument references through args_path-style fields, and the normalized fs-mcp response envelope.

## Status

- 21 MCP tools are exposed through tools/list and covered by the tool matrix integration test.
- The server handles initialize, tools/list, tools/call, resources/list, and resources/templates/list.
- Filesystem, search, and git tools run in Rust code paths.
- Git operations avoid the git CLI for the implemented surface.
- Search and large line/list reads call project-bundled Rust tool executables from the build output, not PATH.
- Resources are currently empty because this project focuses on tool parity first.

## Tool Surface

| Area | Tools |
| --- | --- |
| Files and directories | file-read, file-lines, file-write, dir-mk, dir-list |
| File mutation and metadata | file-copy, file-move, file-remove, file-infos, file-edit |
| Search | search-start, search-regex, search-get, search-stop |
| Git | git-cwd, git-status, git-add, git-commit, git-diff, git-show |
| Inspect | fs-inspect |

## Runtime Model

The binary runs as a stdio server. Each input line is one JSON-RPC request, and each response is written as one line.

```powershell
cargo run --release
```

Example initialize request:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}
```

Example tool listing request:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
```

Example tool call:

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"dir-list","arguments":{"items":[{"path":"."}]}}}
```

## Configuration

Runtime configuration is held in process memory.

| Key | Purpose |
| --- | --- |
| allowedDirectories | Restricts local filesystem and cwd-aware process access to configured roots. Empty means unrestricted. |
| blockedCommands | Reserved for internal process lifecycle helpers. It is not exposed as a public MCP tool control. |
| defaultShell | Reserved for internal process lifecycle helpers. |
| RUST_FS_MCP_TOOL_PROFILE | Optional process env profile. Use fast-coding to expose only fs-inspect in tools/list. |

allowedDirectories can also be seeded from FS_MCP_ALLOWED_DIRECTORIES. Values use the platform path-list separator.
On Windows the default shell is powershell. On other platforms it is sh.

Default blocked command names are rm, rmdir, del, erase, format, mkfs, diskpart, shutdown, reboot, halt, and
poweroff.

## Response Envelope

Every tool call is normalized through the same envelope:

- content contains display text for MCP clients.
- structuredContent.data.content contains normalized content blocks.
- structuredContent.data.structuredContent contains tool-specific structured data.
- structuredContent.status is success or error.
- structuredContent.schemaVersion is 1.
- _meta.fsMcpResult mirrors status, duration, content type, and structured-content presence.
- isError is set on tool failures.

Batch tools return result indexes, original input snippets, per-item status, succeededCount, failedCount, and totalCount.

## Module Layout

| Path | Responsibility |
| --- | --- |
| src/main.rs | Binary entry point. Runs the stdio MCP server and exits non-zero on fatal startup errors. |
| src/lib.rs | Public module exports that preserve stable internal call paths. |
| src/protocol/server.rs | Line-based JSON-RPC handling, initialize response, tool calls, empty resources. |
| src/protocol/catalog.rs | MCP tool catalog, tool annotations, and JSON input schemas. |
| src/core/args_ref.rs | args_path, args_offset, and args_length resolution for large JSON arguments. |
| src/core/batch.rs | Batch execution result shape and per-item summaries. |
| src/core/bundled.rs | Resolver and timeout wrapper for project-bundled rg.exe, fd.exe, and bat.exe. |
| src/core/config.rs | Runtime config, path normalization, allowed-directory checks, blocked commands. |
| src/core/response.rs | RawResult, display text, sanitization, timing, and envelope normalization. |
| src/tools/fs_tools.rs | File, directory, metadata, exact edit, image, and HTTP read tools. |
| src/tools/search_tools.rs | File and content search sessions with regex, literal, context, and pagination support. |
| src/tools/inspect_tools.rs | Compact read-only filesystem inspection requests for coding tasks. |
| src/tools/git_tools.rs | Git cwd, status, add, commit, diff, and show implemented from repository files. |
| src/tools/process_tools.rs | Process stdin interaction for known sessions. Lifecycle helpers are internal and not public MCP tools. |
| tests/tool_matrix.rs | Integration check that every catalog tool is callable through dispatch. |

See ARCHITECTURE.md for the detailed request flow and module contracts.

## Filesystem Tools

Filesystem tools resolve paths through the runtime config boundary before reading or writing. Relative paths are resolved
against the current process directory, home paths beginning with ~ are expanded, and lexical components are normalized.

Supported behavior includes:

- Text, binary, image, and directory reads.
- 1-based line reads with offset and length. file-lines uses native Rust streaming.
- Rewrite and append writes.
- Directory creation and listing with depth, maxEntries, includeFiles, excludePatterns, and allowMissing. dir-list uses native Rust traversal and falls back to bundled fd.exe when excludePatterns are supplied.
- Copy, move, recursive remove, metadata reads, and exact block replacement.
- HTTP URL reads for http:// URLs with redirect handling.

## Search Tools

search-start stores results in an in-memory session and returns a session id. search-get pages through stored results,
and search-stop removes sessions.

Search supports:

- searchType: content or files.
- Regex or literal content search.
- ignoreCase, contextLines, includeHidden, filePattern, and maxResults.
- Binary-file skipping for content search.
- content search uses bundled rg.exe, and file search uses bundled fd.exe.
- The bundled executables are copied from vendor/tools into target/<profile>/tools during the Cargo build.

search-regex runs the same search path without storing a session.

## Git Tools

Git tools discover the repository from path, the pinned git-cwd, or the current directory. The implementation reads and
writes repository files directly.

Implemented behavior includes:

- git-cwd with optional repository initialization.
- git-status from HEAD, index, and worktree comparisons.
- git-add by writing index v2 entries and loose blob objects.
- git-commit by writing tree and commit objects and updating HEAD.
- git-diff for staged, worktree, target, and source/target comparisons.
- git-show for commits, trees, blobs, and file content at revisions.

Commit messages must start with an English Conventional Commit header.

## Development

Run the focused checks before changing behavior:

```powershell
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build
```

The test suite currently includes unit coverage for core behavior and an integration test that verifies the full public
tool matrix.

## Known Limitations

- HTTPS URL reads are rejected until a TLS-capable Rust HTTP client layer is added.
- Git object access reads loose objects. packed-refs are supported for refs, but packfile object storage is not.
- Git index support is version 2.
- Submodules and rename-aware diffs are not implemented.
- Process lifecycle controls are not public MCP tools in the fs-mcp-compatible catalog.
- MCP resources and resource templates currently return empty lists.
