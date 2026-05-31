# rust-fs-mcp

rust-fs-mcp is a Rust stdio MCP server that ports the public fs-mcp tool contracts to a native Rust
implementation. It exposes filesystem, search, and git tools through
line-oriented JSON-RPC over stdin/stdout.

The project keeps the Node fs-mcp contract shape where it matters: public tool names, batch-first inputs,
large-argument references through args_path-style fields, and the normalized fs-mcp response envelope.

## Status

- 22 MCP tools are exposed through tools/list and covered by the tool matrix integration test.
- The server handles initialize, tools/list, tools/call, resources/list, and resources/templates/list.
- Filesystem and inspection tools run in native Rust code paths.
- Search and git tools wrap external CLI tools resolved from PATH.
- rg, fd, and git must be installed and resolvable on PATH for search, exclude-aware listing, and git tools.
- Resources are currently empty because this project focuses on tool parity first.

## Install

Prebuilt binaries are published for every tagged release. Pick the asset that matches
your platform from the [latest release](https://github.com/jungho-dev/rust-fs-mcp/releases/latest),
or use the direct URL pattern:

```
https://github.com/jungho-dev/rust-fs-mcp/releases/download/<tag>/rust-fs-mcp-<target>.zip
```

| OS | Architecture | Target triple |
| --- | --- | --- |
| Windows | x86_64 | `x86_64-pc-windows-msvc` |
| Windows | aarch64 | `aarch64-pc-windows-msvc` |
| macOS | x86_64 (Intel) | `x86_64-apple-darwin` |
| macOS | aarch64 (Apple Silicon) | `aarch64-apple-darwin` |
| Linux | x86_64 | `x86_64-unknown-linux-gnu` |
| Linux | aarch64 | `aarch64-unknown-linux-gnu` |

Each archive ships with a matching `<asset>.sha256sum` file. A source tarball
`rust-fs-mcp_src.tar.gz` is also attached to every release.

Verify before extracting:

```bash
# Unix
shasum -a 256 -c rust-fs-mcp-x86_64-unknown-linux-gnu.zip.sha256sum
```

```powershell
# Windows PowerShell
(Get-FileHash -Algorithm SHA256 .\rust-fs-mcp-x86_64-pc-windows-msvc.zip).Hash
# compare against the contents of the .sha256sum file
```

Releases are produced by `.github/workflows/release.yml`. The workflow funnels three event
shapes through a single `resolve` job:

- `git push origin main` reads the `version` field of `Cargo.toml`. If `v<version>` does not
  exist on origin yet, the workflow creates and pushes that tag, then publishes the release.
  Pushes whose `v<version>` tag already exists are no-ops.
- `git push origin v<X.Y.Z>` releases that exact tag.
- A manual `workflow_dispatch` with an explicit `tag` input releases that tag.

In the auto-tag path the workflow commits the tag as `github-actions[bot]` and uses the
default `GITHUB_TOKEN`, so no extra secrets are required.

## Build From Source

```powershell
cargo build --release
# binary at: target/release/rust-fs-mcp (rust-fs-mcp.exe on Windows)
```

To cross-build a specific target locally, install the target and pass `--target`:

```powershell
rustup target add aarch64-apple-darwin
cargo build --release --target aarch64-apple-darwin
```

## Tool Surface

| Area | Tools |
| --- | --- |
| Files and directories | file-read, file-lines, file-write, dir-mk, dir-list |
| File mutation and metadata | file-copy, file-move, file-remove, file-infos, file-edit, file-edit-lines |
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
| RUST_FS_MCP_TOOL_PROFILE | Optional process env profile. Use fast-coding to expose only fs-inspect in tools/list. |
| RUST_FS_MCP_COMPACT | Default on. Drops the data.text copy of content blocks to save client tokens. Set 0 or false to restore data.text. |
| RUST_FS_MCP_READ_MAX_CHARS | Whole-file file-read character cap (default 100000). Larger reads are truncated with a truncated flag; pass offset/length to page. 0 disables. |
| RUST_FS_MCP_BATCH_WORKERS | Optional cap on the per-process batch worker count. A positive integer limits concurrency; unset or invalid falls back to available parallelism (or 4). |

allowedDirectories can also be seeded from the RUST_FS_MCP_ALLOWED_DIRECTORIES environment variable using the platform path-list separator.

## Response Envelope

Every tool call is normalized through the same envelope:

- content contains display text for MCP clients.
- structuredContent.data.content contains normalized content blocks; file bodies are carried here.
- structuredContent.data.structuredContent contains tool-specific structured data; for reads this is metadata only and no longer duplicates the file body.
- structuredContent.data.text duplicates data.content and is emitted only when RUST_FS_MCP_COMPACT is disabled.
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
| src/core/external.rs | Wrapper that spawns external CLI tools (rg, fd, git) resolved from PATH with timeouts and stdout/stderr capture. |
| src/core/config.rs | RuntimeConfig (allowedDirectories), path normalization, home expansion, lexical normalization, and allowedDirectories enforcement with a path-allowed cache. |
| src/core/response.rs | RawResult, display text, sanitization, timing, and envelope normalization. |
| src/tools/fs_tools.rs | File, directory, metadata, exact block edit (file-edit), 1-based line edit (file-edit-lines), image, and HTTP read tools. |
| src/tools/search_tools.rs | File and content search sessions with regex, literal, context, and pagination support. |
| src/tools/inspect_tools.rs | Compact read-only filesystem inspection requests for coding tasks. |
| src/tools/git_tools.rs | Git cwd, status, add, commit, diff, and show that wrap the git CLI resolved from PATH. |
| tests/tool_matrix.rs | Integration check that every catalog tool is callable through dispatch. |

See ARCHITECTURE.md for the detailed request flow and module contracts.

## Filesystem Tools

Filesystem tools resolve paths through the runtime config boundary before reading or writing. Relative paths are resolved
against the current process directory, home paths beginning with ~ are expanded, and lexical components are normalized.

Supported behavior includes:

- Text, binary, image, and directory reads.
- 1-based line reads with offset and length. file-lines uses native Rust streaming.
- Rewrite and append writes.
- Directory creation and listing with depth, maxEntries, includeFiles, excludePatterns, and allowMissing. dir-list uses native Rust traversal and falls back to fd from PATH when excludePatterns are supplied.
- Copy, move, recursive remove, metadata reads, exact block replacement (file-edit), and 1-based line-range replacement (file-edit-lines).
- HTTP URL reads for http:// URLs with redirect handling.

## Search Tools

search-start stores results in an in-memory session and returns a session id. search-get pages through stored results,
and search-stop removes sessions.

Search supports:

- searchType: content or files.
- Regex or literal content search.
- ignoreCase, contextLines, includeHidden, filePattern, and maxResults.
- Binary-file skipping for content search.
- content search shells out to ripgrep (rg) and file search shells out to fd. Both tools are resolved from PATH and must be installed.
- The internal ExternalTool enum wraps exactly three PATH-resolved binaries: rg, fd, and git.

search-regex runs the same search path without storing a session.

## Git Tools

Git tools discover the repository from path or the pinned git-cwd, then invoke the git CLI resolved from PATH inside the
resolved worktree.

Implemented behavior includes:

- git-cwd resolves the worktree through rev-parse and can run git init first when requested.
- git-status runs status --porcelain --branch and returns the porcelain lines.
- git-add stages paths through git add.
- git-commit injects a default committer identity (user.name=rust-fs-mcp, user.email=rust-fs-mcp@example.invalid) so commits work without local git config, accepts an optional author override, and supports amend and allow-empty.
- git-diff runs git diff with optional staged, name-only, stat, source/target, and path filters.
- git-show renders an object or object:path through git show.

Commit messages must start with an English Conventional Commit header.

## Inspect Tool

fs-inspect answers several read-only questions about a directory tree in one batched call. It takes a root and a list of
requests and returns one answer per request with status, confidence, evidence snippets, and aggregate metrics. The shared
maxSnippetChars budget (default 6000) caps evidence text so large scans stay token-bounded.

Supported request ops:

- count-files: count files matching a glob, with optional recursion and sample paths.
- search: regex or literal content search with optional field extraction and per-file pattern filtering.
- json-pick: read a JSON file and return values at the given JSON pointers.
- snippet: return context-bounded snippets around lines that contain any of the given patterns.
- git-status: fold a git-status lookup into the same call so reads, searches, and git state resolve in one round-trip.

RUST_FS_MCP_TOOL_PROFILE=fast-coding limits tools/list to fs-inspect only.

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
- Git tools require a git binary on PATH; there is no in-process git object store.
- Git behavior follows the installed git CLI, including its submodule and rename-detection defaults.
- MCP resources and resource templates currently return empty lists.
