# rust-fs-mcp

rust-fs-mcp is a Rust stdio MCP server that ports the public fs-mcp tool contracts to a native Rust
implementation. It exposes filesystem, search, and git tools through
line-oriented JSON-RPC over stdin/stdout.

The project keeps the Node fs-mcp contract shape where it matters: public tool names, batch-first inputs,
large-argument references through args_path-style fields, and the normalized fs-mcp response envelope.

## Status

- 23 MCP tools are exposed through tools/list and covered by the tool matrix integration test.
- The server handles initialize, tools/list, tools/call, resources/list, and resources/templates/list.
- Filesystem and inspection tools run in native Rust code paths.
- Content search runs in-process on ripgrep's own libraries (grep-searcher + ignore); no rg binary is required.
- Git tools wrap the external git CLI resolved from PATH.
- fd and git must be installed and resolvable on PATH for exclude-aware listing and git tools.
- web-fetch, web-extract, and download-to-file run on a native tokio-free HTTPS client (ureq); web-render optionally shells out to an installed obscura(-like) headless-browser CLI for JS/SPA rendering.
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
| Files and directories | file-read, file-read-line-range, file-write, dir-create, dir-list |
| Path mutation and metadata | path-copy, path-move, path-remove, path-stat, file-edit, file-edit-lines |
| Search | fs-search |
| Git | git-status, git-add, git-commit, git-amend, git-diff, git-show |
| Inspect | fs-inspect |
| Web | web-fetch, web-render, web-extract, download-to-file |

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

## Fixed Behavior

Runtime behavior is not configurable through project environment variables or process-global settings.

- The full 23-tool catalog is always exposed; the fixed always-load annotations remain on the core file, search, edit, and git read tools.
- Responses always use the compact envelope: `{data, durationMs}` plus `error` only on failure.
- Whole-file reads are capped at 100,000 characters; use `offset` and `length` to page larger files.
- Batch plans use fixed workload limits (read 3/8, stat 4/16, search 2/2, fetch 2/32, download 2/16).
- `fs-inspect` uses a fixed 25-second internal deadline.
- Local filesystem paths are resolved without an allowed-root policy. Git tools require an explicit `path` on every call.
- The SSRF guard blocks non-public addresses except loopback (localhost/127.0.0.0/8/::1), which is allowed for local development; `web-render` always rejects `evalScript`.
- External CLIs, including `obscura`, are resolved from PATH.

## Response Envelope

Every tool call is normalized through the same envelope:

- content contains display text for MCP clients.
- structuredContent.data.content contains normalized content blocks; result bodies (file contents, search lines, diffs, listings) are carried here exactly once.
- structuredContent.data.structuredContent contains tool-specific structured metadata only (counts, paths, backends); it never duplicates the body.
- structuredContent.durationMs is the tool duration.
- structuredContent.error appears only on failure with {message}.
- The compact envelope is always used; it omits data.text, error:null, schemaVersion, status, and toolName on successful calls.
- _meta.fsMcpResult mirrors status, duration, content type, and structured-content presence.
- isError is set on tool failures.

Batch tools return per-item {index, ok, data} entries plus succeededCount, failedCount, and totalCount; the full envelope restores per-item {index, input, ok, result} entries with the verbatim request echo.

## Module Layout

| Path | Responsibility |
| --- | --- |
| src/main.rs | Binary entry point. Runs the stdio MCP server and exits non-zero on fatal startup errors. |
| src/lib.rs | Public module exports that preserve stable internal call paths. |
| src/protocol/server.rs | Line-based JSON-RPC handling, initialize response, tool calls, empty resources. |
| src/protocol/catalog.rs | MCP tool catalog, tool annotations, and JSON input schemas. |
| src/core/args_ref.rs | args_path, args_offset, and args_length resolution for large JSON arguments. |
| src/core/batch.rs | Sequential, pooled-parallel, and mutation-safe batch execution plus the result shape and per-item summaries. |
| src/core/external.rs | Wrapper that spawns external CLI tools (fd, git) resolved from PATH with timeouts and stdout/stderr capture. |
| src/core/config.rs | Path normalization, home expansion, lexical normalization, and direct path resolution. |
| src/core/response.rs | RawResult, display text, timing, envelope normalization, and the (currently passthrough) sanitizer seam. |
| src/core/web.rs | Tokio-free HTTPS fetch (ureq), the per-hop SSRF guard, the body-size cap, and HTML extraction (html2text, htmd, scraper, dom_smoothie). |
| src/tools/fs_tools.rs | File, directory, metadata, exact block edit (file-edit), 1-based line edit (file-edit-lines), image, and file-read isUrl (delegates to core::web) tools. |
| src/tools/search_tools.rs | In-process content regex search on grep-searcher + ignore (backend `native-grep`). |
| src/tools/inspect_tools.rs | Compact read-only filesystem inspection requests for coding tasks. |
| src/tools/git_tools.rs | Git cwd, status, add, commit, amend, diff, and show that wrap the git CLI resolved from PATH. |
| src/tools/web_tools.rs | web-fetch, web-render, web-extract, and download-to-file handlers. |
| tests/tool_matrix.rs | Integration check that every catalog tool is callable through dispatch. |

See architecture.md for the detailed request flow and module contracts.

## Filesystem Tools

Filesystem tools resolve paths through the runtime config boundary before reading or writing. Relative paths are resolved
against the current process directory, home paths beginning with ~ are expanded, and lexical components are normalized.

Supported behavior includes:

- Text, binary, image, and directory reads.
- Local text line-range reads with 1-based start_line and optional line_count. file-read-line-range uses native Rust streaming.
- Rewrite and append writes.
- Directory creation and listing with depth, maxEntries, includeFiles, excludePatterns, and allowMissing. dir-list uses native Rust traversal and falls back to fd from PATH when excludePatterns are supplied.
- Copy, move, recursive remove, metadata reads, exact block replacement (file-edit, which rejects an empty old_string), and 1-based line-range replacement (file-edit-lines, which preserves the original line endings including a missing final newline).
- file-read isUrl: true reads HTTP/HTTPS URLs through the shared core::web client: per-hop SSRF guard, redirect following, and a body-size cap. See Web Tools below for the dedicated web-fetch/web-render/web-extract/download-to-file tools.

## Search Tools

fs-search runs a ripgrep-compatible regular-expression content search and returns the batch result directly.

Search supports:

- ignoreCase, contextLines, includeHidden, filePattern, and maxResults.
- Binary-file skipping for content search.
- pattern_path indirection for large patterns and filePattern to narrow the target files.
- content search runs in-process on grep-searcher + ignore (ripgrep's own libraries), so rg does not need to be installed; the structured result reports backend `native-grep`.

## Git Tools

Git tools require a `path` for every call, then invoke the git CLI resolved from PATH inside the resolved worktree.

Implemented behavior includes:

- git-status runs status --porcelain --branch and returns the porcelain lines.
- git-add stages paths through git add and forwards all (--all), update (--update), and force (--force); all and update stage changes without an explicit pathspec.
- git-commit injects a default committer identity (user.name=rust-fs-mcp, user.email=rust-fs-mcp@example.invalid) so commits work without local git config, accepts an optional author override, and supports amend, allow-empty, and no-verify.
- git-amend rewrites the last commit: it reuses the existing message with --no-edit when no message is given, otherwise validates a new Conventional Commit message, and supports an author override or reset-author (mutually exclusive), staging files first, allow-empty, and no-verify.
- git-diff runs git diff with optional staged, name-only, stat, source/target, contextLines (mapped to --unified=<n>), check (mapped to --check, flagging whitespace errors and leftover conflict markers), and path filters.
- git-show renders an object or object:path through git show.
- git-diff (source/target) and git-show (object/objects) reject revision values that begin with -, so a revision cannot smuggle git options such as --output.

Commit messages must start with a Conventional Commit header (lowercase English type; the summary may be any language).

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

Directory traversal for count-files and search does not follow symlinks or Windows junctions, so reparse-point cycles cannot cause unbounded recursion.

## Web Tools

The web tier is a two-tier design: a native fetch path for static content and an external headless-browser path for JS-rendered pages.

- web-fetch (TIER-1): native ureq blocking HTTPS client with no async runtime. Batches items[] or a single url, and dumps html, text, markdown, links, or readability (main-content extraction).
- web-render (TIER-2): shells out to an obscura(-like) headless-browser CLI resolved from PATH for JavaScript/SPA pages, with selector, wait, waitUntil, and stealth. Try web-fetch first; escalate only when the page needs JS execution. `evalScript` is disabled.
- web-extract: converts HTML you already hold (inline or a local file) into text, markdown, links, or readability, fully offline.
- download-to-file: downloads a URL to the requested resolved local path.

SSRF guard: web-fetch, download-to-file, and file-read isUrl resolve the host and reject private, link-local, unique-local, CGNAT, multicast/reserved, and embedded-IPv4 IPv6 addresses (mapped, compatible, NAT64, 6to4), re-checked on every redirect hop. Loopback (localhost/127.0.0.0/8/::1) is allowed so local development servers can be reached, while embedded-IPv4 forms of loopback stay blocked. The validated addresses are pinned into the connection resolver, so the socket always connects to the checked IPs (no DNS-rebinding window). web-render rejects `evalScript` because it can bypass this guard.

Body size is capped per request (maxBytes, default 5,000,000 for fetch and 50,000,000 for download) and hard-clamped to 200,000,000 bytes regardless of the requested value.

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

- Git tools require a git binary on PATH; there is no in-process git object store.
- Git behavior follows the installed git CLI, including its submodule and rename-detection defaults.
- web-render requires a separately installed obscura(-like) headless-browser binary; without it, JS-rendered pages cannot be fetched.
- The SSRF guard checks the resolved address at request time; it does not defend against DNS rebinding between resolution and connection.
- MCP resources and resource templates currently return empty lists.
