# rust-fs-mcp

A native Rust stdio [MCP](https://modelcontextprotocol.io) server that gives an AI agent one fast,
batch-first toolset for local **files, search, git, and the web**. It speaks JSON-RPC over
stdin/stdout and ships as a single self-contained binary.

The tool names and request shapes follow the public fs-mcp contract, so it drops into any MCP client
that already understands that surface.

## Why rust-fs-mcp

- **One binary, almost no dependencies.** No Node, Python, or `rg` runtime. Only `git` on PATH (for
  the git tools) and, optionally, a headless-browser CLI (for `web-render`) are external.
- **23 tools** across files, directories, path operations, content search, git, filesystem
  inspection, and web fetch.
- **Batch-first.** Same-kind operations take an `items[]` (or `paths[]`) array and run on a pooled
  parallel executor, so an agent reads or edits many targets in one call.
- **In-process search.** Content search uses ripgrep's own libraries (grep-searcher + ignore); no
  `rg` binary is required.
- **Safe web access.** A tokio-free HTTPS client fetches URLs behind a per-hop SSRF guard.
- **Predictable output.** Every result uses one compact envelope, size-bounded to stay under an MCP
  client's output-token cap.

## Quick Start

1. Get the binary from [Install](#install) (or [Build From Source](#build-from-source)).
2. Register it with your MCP client. The server takes **no arguments**; it communicates over
   stdin/stdout. Most clients (Claude Code, Claude Desktop, and others) use this shape:

   ```json
   {
     "mcpServers": {
       "rust-fs-mcp": {
         "command": "/absolute/path/to/rust-fs-mcp"
       }
     }
   }
   ```

   On Windows, point `command` at the full path to `rust-fs-mcp.exe` (escape backslashes in JSON, or
   use forward slashes):

   ```json
   {
     "mcpServers": {
       "rust-fs-mcp": {
         "command": "C:/tools/rust-fs-mcp/rust-fs-mcp.exe"
       }
     }
   }
   ```

3. Restart the client. The 23 tools appear in `tools/list`.

The server negotiates the MCP protocol version automatically (it supports `2024-11-05`,
`2025-03-26`, and `2025-06-18`), so no version setting is needed on the client.

## Tools

| Area | Tools |
| --- | --- |
| Files and directories | `file-read`, `file-read-line-range`, `file-write`, `dir-create`, `dir-list` |
| Path operations and metadata | `path-copy`, `path-move`, `path-remove`, `path-stat`, `file-edit`, `file-edit-lines` |
| Search | `fs-search` |
| Git | `git-status`, `git-add`, `git-commit`, `git-amend`, `git-diff`, `git-show` |
| Inspect | `fs-inspect` |
| Web | `web-fetch`, `web-render`, `web-extract`, `download-to-file` |

Read-only tools (`file-read`, `file-read-line-range`, `dir-list`, `fs-search`, `path-stat`,
`fs-inspect`, `git-status`, `git-diff`, `git-show`, `web-fetch`, `web-render`, `web-extract`) carry
the MCP `readOnlyHint`. Mutating tools (`file-write`, `file-edit`, `file-edit-lines`, `path-move`,
`path-remove`, `git-commit`, `git-amend`, `download-to-file`) carry `destructiveHint`.

## Usage Essentials

**Batch-first input.** Pass every same-kind target in one call:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
  "name":"file-read",
  "arguments":{"paths":["src/main.rs","src/lib.rs","Cargo.toml"]}
}}
```

Keep read batches near 3-8 files and split larger sets. Oversized items are truncated per item, not
hard-rejected.

**Forgiving arguments.** The dispatcher absorbs common shape slips before a tool runs: a single flat
operation is wrapped into `items[]`, key aliases are normalized (`file_path`->`path`,
`from`/`to`->`source`/`destination`), `path-remove` and the metadata tools accept a plain `paths[]`
array as well as `items[]`, and an array sent as a JSON-encoded string is parsed back into an array.

**Paths.** Prefer absolute paths. Relative paths resolve against the process working directory, and a
leading `~` expands to the home directory. There is no allowed-root restriction; git tools require an
explicit `path` on every call.

**Large arguments.** Any tool can take its arguments from a file with
`{"args_path":"/abs/path/to/args.json"}` (with optional `args_offset`/`args_length`), which keeps a
big payload out of the JSON-RPC line.

## Response Envelope

Every call is normalized to one compact envelope: `{data, durationMs}`, plus `error` only on failure.

- `data.content` carries the result body (file contents, search lines, diffs, listings) exactly once.
- `data.structuredContent` carries tool-specific metadata only (counts, paths, backends); it never
  duplicates the body.
- `durationMs` is the tool's wall-clock duration.
- `_meta.fsMcpResult` mirrors status, duration, content type, and whether structured content is
  present.
- `isError` is set on tool failures.

Batch tools add per-item `{index, ok, data}` entries plus `succeededCount`, `failedCount`, and
`totalCount`.

**Output size.** A result is bounded server-side to `MAX_STANDARD_BYTES` (30,000 bytes) so it stays
under a client's MCP output-token cap (for example Claude Code's 25,000-token default). An oversized
body is truncated in place with a notice and `outputTruncated`, rather than overflowing the client.
To keep full fidelity, request a smaller slice: `offset`/`length`, `maxResults`, or fewer batch
items.

## Install

Prebuilt binaries are attached to every [release](https://github.com/jungho-dev/rust-fs-mcp/releases/latest).
Download the archive for your platform, or use the direct URL pattern:

```text
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

Each archive ships a matching `<asset>.sha256sum`. Verify before extracting:

```bash
# Unix
shasum -a 256 -c rust-fs-mcp-x86_64-unknown-linux-gnu.zip.sha256sum
```

```powershell
# Windows PowerShell
(Get-FileHash -Algorithm SHA256 .\rust-fs-mcp-x86_64-pc-windows-msvc.zip).Hash
# compare against the contents of the .sha256sum file
```

## Build From Source

Requires Rust 1.85+ (edition 2024).

```powershell
cargo build --release
# binary at: target/release/rust-fs-mcp (rust-fs-mcp.exe on Windows)
```

To cross-build a specific target, install it and pass `--target`:

```powershell
rustup target add aarch64-apple-darwin
cargo build --release --target aarch64-apple-darwin
```

## Tool Reference

### Files and directories

- `file-read` reads text, binary, image, and directory targets in parallel. Each item takes an
  optional `offset`/`length` slice; `isUrl: true` fetches an HTTP/HTTPS URL through the shared web
  client (see [Web](#web)).
- `file-read-line-range` returns a 1-based line range (`start_line`, optional `line_count`) with line
  numbers, using native streaming.
- `file-write` rewrites or appends (`mode`); large content can come from `content_path`.
- `dir-create` creates one or many directories.
- `dir-list` lists directories with `depth`, `maxEntries`, `includeFiles`, `excludePatterns`, and
  `allowMissing`. It hides `node_modules/`, `target/`, and `.git/` by default unless the listed path
  is inside one; `noDefaultExcludes: true` lists them.

### Path operations and metadata

- `path-copy`, `path-move`, `path-remove` copy, move/rename, and delete with `recursive`/`force`
  flags. Independent paths run in parallel; overlapping paths fall back to a sequential runner.
- `path-stat` returns metadata for many paths at once.
- `file-edit` applies exact block replacements (`old_string` -> `new_string`), can enforce
  `expected_replacements`, and rejects an empty `old_string`.
- `file-edit-lines` replaces, inserts, or deletes by 1-based line number, preserving the file's
  original line endings (including a missing final newline).

### Search

`fs-search` runs ripgrep-compatible regex content search in-process and returns the batch result
directly.

- Flags: `ignoreCase`, `literal` (fixed string, `rg -F`), `wordMatch` (`rg -w`), `multiline`
  (`rg -U`), `contextLines`, `includeHidden`, `filePattern`, `maxResults`.
- Pattern flavor is Rust regex: a linear-time engine with Unicode-aware `\d \w \b`, so Hangul word
  boundaries work. Look-around and backreferences transparently switch to a backtracking engine
  (fancy-regex, bounded by a backtrack limit and the search timeout); a plain syntax error falls back
  to a literal search once.
- Binary files are skipped. Large patterns can come from `pattern_path`. The backend label is
  reported in the structured result (for example `native-grep`).

### Git

Every git tool requires a `path`, then runs the git CLI resolved from PATH inside that worktree.

- `git-status` runs `status --porcelain --branch`.
- `git-add` stages paths, with `all`/`update`/`force`.
- `git-commit` injects a default committer identity so commits succeed without local git config,
  accepts an optional author override, and supports `amend`/`allow-empty`/`no-verify`. The message
  must start with a Conventional Commit header (lowercase English type; the summary may be any
  language).
- `git-amend` rewrites the last commit: `--no-edit` when no message is given, otherwise a validated
  new message; supports author override or reset-author, staging, `allow-empty`, and `no-verify`.
- `git-diff` runs `git diff` with `staged`, `nameOnly`, `stat`, `source`/`target`, `contextLines`,
  `check`, and path filters.
- `git-show` renders an object (or `objects[]`) and optional `object:path` through `git show`.

Revision inputs that begin with `-` are rejected, so a revision cannot smuggle a git option.

### Inspect

`fs-inspect` answers several read-only questions about a directory tree in one batched call. It takes
a `root` and a list of requests, returning one answer each with status, confidence, evidence
snippets, and aggregate metrics. A shared `maxSnippetChars` budget (default 6000) keeps evidence
token-bounded.

Request ops: `count-files`, `search`, `json-pick` (values at JSON pointers), `snippet`, and
`git-status` (folded in so filesystem and git state resolve in one round-trip). Traversal does not
follow symlinks or Windows junctions.

### Web

A two-tier design: a native path for static content and an external headless-browser path for
JavaScript-rendered pages.

- `web-fetch` (native): fetches one or many URLs over HTTP/HTTPS with no browser and dumps `markdown`
  (default), `text`, `links`, `readability` (main content), or raw `html`. Try this first.
- `web-render` (external): renders one URL in an obscura(-like) headless-browser CLI resolved from
  PATH for JS/SPA pages, with `selector`, `wait`, `waitUntil`, and `stealth`. Escalate here only when
  the page needs JS execution.
- `web-extract`: converts HTML you already hold (inline or a local file) into markdown, text, links,
  or readability, fully offline.
- `download-to-file`: downloads one or many URLs to local files, streamed to a temp file and renamed
  into place.

**SSRF guard.** `web-fetch`, `download-to-file`, and `file-read isUrl` resolve the host and reject
non-public addresses (private, link-local, unique-local, CGNAT, multicast/reserved, and
embedded-IPv4 IPv6 forms), re-checking on every redirect hop. Loopback (`localhost`/`127.0.0.0/8`/
`::1`) is allowed for local development. Validated addresses are pinned into the connection resolver.
`web-render` rejects `evalScript` because in-browser requests can bypass the guard.

Body size defaults to a 200,000,000-byte hard ceiling for both fetch and download; an explicit
`maxBytes` can only lower it.

## Requirements and Limitations

- `git` must be on PATH for the git tools; there is no in-process git object store, and behavior
  follows the installed git version (including its submodule and rename-detection defaults).
- `web-render` needs a separately installed obscura(-like) headless-browser binary; without it,
  JS-rendered pages cannot be fetched.
- The SSRF guard checks the resolved address at request time; it does not defend against DNS
  rebinding between resolution and the TCP connect.
- MCP resources and resource templates currently return empty lists; this project focuses on tools.

## Development

```powershell
cargo test
cargo clippy --all-targets
cargo build
```

The suite includes unit coverage for core behavior and an integration test (`tests/tool_matrix.rs`)
that verifies every catalog tool is callable through dispatch.

For a module-by-module tour of the request flow and internal contracts, see
[architecture.md](architecture.md). A Korean translation of this document is in
[readme-ko.md](readme-ko.md).

## License

Apache-2.0. See [LICENSE.md](LICENSE.md).