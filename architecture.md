# Architecture

This document describes the current rust-fs-mcp architecture as implemented in the source tree.

## Goals

rust-fs-mcp is designed around five constraints:

- Keep public fs-mcp tool names and request shapes stable.
- Keep all tool results inside a normalized response envelope.
- Resolve external CLI tools (git, obscura) from PATH instead of bundling them with the binary; content search runs in-process on ripgrep's own libraries, so no search binary is required.
- Keep state explicit and process-local (HTTP agent cache, git toplevel cache); there is no session or workdir state.
- Keep the web fetch tier synchronous and tokio-free; delegate JavaScript rendering to an external headless-browser CLI instead of embedding a browser engine.

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
4. Requests without an id (notifications) return no response; a request that carries an id always receives one. A malformed or non-UTF-8 input line returns a JSON-RPC parse error without terminating the server.
5. initialize negotiates the protocol version (a supported requested version is echoed, an unknown one is answered with the latest supported version) and returns capabilities, server info, and the fixed batch-first server instructions for every client.
6. tools/list returns catalog entries and input schemas.
7. tools/call extracts params.name and params.arguments and runs on a per-request worker thread, so a slow tool (web-render, large search, git) does not block other requests; responses are serialized through a shared writer lock and matched by JSON-RPC id.
8. tools::dispatch_tool_call resolves args_path, args_offset, and args_length.
9. The concrete tool handler returns RawResult.
10. core::response::normalize_tool_result builds the MCP content, structuredContent, _meta, and isError fields.

## Module Responsibilities

| Module | Role |
| --- | --- |
| main | Binary entry point and fatal error handling. |
| lib | Re-exports core, protocol, and tools modules. |
| protocol::server | JSON-RPC line protocol, method routing, protocol-version negotiation, per-request tools/call worker dispatch with a shared response writer, empty resource handlers. |
| protocol::catalog | Public tool registry, tool descriptions, annotations, and JSON schemas. |
| core::args_ref | Large argument indirection through args_path and optional character slicing. |
| core::batch | Shared batch execution (sequential, pooled-parallel with per-workload plans, mutation conflict analysis) and structured batch result format. |
| core::external | Spawns external CLI tools (git, obscura) resolved from PATH and captures stdout/stderr with timeouts; the git path is resolved once and de-shuttled (cmd\git.exe -> mingw64\bin\git.exe, ~13ms per spawn). |
| core::config | Home expansion, lexical path normalization, and direct path resolution. |
| core::response | RawResult type, display text, response timing, public envelope normalization, and the (currently passthrough) sanitizer seam. |
| core::web | Tokio-free blocking HTTPS fetch (ureq), the per-hop SSRF guard, the body-size cap, and HTML extraction (html2text, htmd, scraper, dom_smoothie). |
| tools::mod | Tool name dispatcher and cross-tool argument resolution boundary. |
| tools::fs_tools | File, directory, metadata, exact block edit (file-edit), 1-based line edit (file-edit-lines), image, and file-read isUrl (delegates to core::web) behavior. |
| tools::search_tools | In-process content regex search on ripgrep's own libraries (grep-searcher + ignore parallel walk); no rg spawn. |
| tools::inspect_tools | Compact read-only filesystem inspection collection for coding tasks. |
| tools::git_tools | Repository discovery plus git status/add/commit/diff/show by invoking the git CLI resolved from PATH. |
| tools::web_tools | web-fetch (native), web-render (obscura shell-out), web-extract (offline HTML conversion), and download-to-file (sandboxed download) handlers. |
| tests::tool_matrix | End-to-end catalog and dispatch coverage for the public tool surface. |

## Fixed Runtime Model

The server has no project-level runtime configuration or session workdir state. Filesystem paths resolve directly; each git call requires its own `path`.

## Path Boundary

core::config resolves paths without an allowed-root policy.

- ~ is expanded from USERPROFILE or HOME.
- Relative paths are joined to the process current directory.
- Lexical components are normalized.
- `target_path` resolves the target path without a parent allow-list check.
- The full catalog and its always-load annotations are fixed.

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
- The compact envelope is fixed and omits data.text, error:null, schemaVersion, status, and toolName on successful calls.
- _meta.fsMcpResult: compact status metadata.
- isError: present only when the result is an error.

The sanitizer functions in core::response are retained as a seam but currently pass text and JSON
through unchanged (parity with go-fs-mcp, which neutered its end-token rewrite).

## Batch Contract

Batch tools use run_batch, run_batch_parallel, run_batch_mutation, and create_batch_response.
Read-style tools run on a lazy persistent worker pool with an atomic work cursor; the caller
thread always participates, so no OS thread is spawned per request. Per-workload plans gate
parallelism (read 3/8, stat 4/16, search 2/2, fetch 2/32, download 2/16). Mutations run in parallel (2/4) only when every
item touches provably independent paths - same path, ancestor/descendant, unknown shape, or more
than 256 items fall back to the sequential runner. The default compact batch layer preserves:

- 1-based input index.
- Per-item ok flag.
- Per-item data carrying the tool-specific structured metadata (omitted when null).
- failedCount, succeededCount, totalCount, and toolName.

Compact batch entries never retain a verbatim input object or per-item result wrapper.

A batch response is marked as a tool error only when every item fails.

## Filesystem Architecture

fs_tools is split into read, write/directory, copy/move/remove/info/edit, shared helpers, and HTTP helpers.

Important contracts:

- Local paths pass through ensure_path_allowed, existing_path, or target_path.
- Writes create parent directories when needed.
- file-edit performs exact string replacement, can enforce expected_replacements, and rejects an empty old_string.
- file-edit-lines replaces inclusive 1-based line ranges, preserving the file's original line endings including the absence of a trailing newline on the final line.
- Binary files are detected through NUL bytes.
- Image files are returned as image content blocks with base64 data.
- Directory traversal honors depth, maxEntries, includeFiles, excludePatterns, and allowMissing.
- URL reads (isUrl: true) delegate to core::web::http_fetch: HTTP and HTTPS, per-hop SSRF guard, redirect following, and a body-size cap. See Web Architecture below.
- file-read-line-range reads local text ranges through native Rust streaming with a 1-based start line.
- dir-list uses native Rust traversal only; excludePatterns are matched by the built-in compiled wildcard set.

## Search Architecture

fs-search runs a single ripgrep-compatible content search per item and returns the batch result directly.
The engine is in-process: an ignore parallel walk feeds grep-searcher with a regex(bytes) matcher, so the
~16ms per-search rg spawn is gone. Reaching maxResults quits the walk immediately instead of draining the
remaining tree, and the timeout_ms deadline is checked inside the walk and the per-line sink.

Backend:

- Content search runs in-process on grep-searcher + ignore (ripgrep's own libraries); rg does not need to be installed.
- The pattern flavor is Rust regex (linear time, Unicode-aware \d \w \b). Look-around, backreferences, and oversized compilations are rejected with rewrite hints; plain syntax errors fall back to a literal search once with the parse-error gist in the backend label. Unreadable files degrade to a partial result label.
- Result structured data records the backend label (for example `native-grep`).

Search behavior:

- The search reads text-like files and matches with ripgrep regular expressions.
- ignoreCase sets case-insensitive matching.
- literal forces fixed-string matching (rg -F), wordMatch wraps the pattern in Unicode-aware word boundaries (rg -w), and multiline enables cross-line matches with dot-matches-newline (rg -U; reads each searched file fully into memory).
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
- A path is required for every git tool call.
- The worktree root is confirmed with rev-parse --show-toplevel.

Command behavior:

- git-add runs git add for the given paths, and adds --all, --update, or --force when those flags are set (all/update allow staging without an explicit pathspec).
- git-commit always injects -c user.name=rust-fs-mcp and -c user.email=rust-fs-mcp@example.invalid so commits succeed without local git config, adds --author when an author object is given, and forwards amend, allow-empty, and no-verify.
- git-amend rewrites HEAD: it requires an existing commit, reuses the message with --no-edit when none is given, otherwise validates a new Conventional Commit message, rejects combining author with reset-author, and forwards staged files, allow-empty, and no-verify.
- git-status runs git status --porcelain --branch.
- git-diff runs git diff with optional staged, name-only, stat, source/target, contextLines (mapped to --unified=<n>), check (mapped to --check, flagging whitespace errors and leftover conflict markers), and path arguments.
- git-show runs git show over the object or objects[] revision set with optional filePath pairing, stat (diffstat), and format=raw; multiple revisions resolve in one call.

Validation:

- Commit messages must pass a Conventional Commit header check (lowercase English type, any-language summary) before git runs.
- git-diff (source/target) and git-show (object/objects) reject revision values beginning with - before git runs, so a revision cannot be parsed as a git option such as --output.
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
- search scans with the same grep-searcher line-mode engine as fs-search (whole-buffer scan, `crlf` regex mode so `$` still matches CRLF lines, NUL files skipped as binary): target files are collected in sorted path order, scanned in parallel on the shared worker pool, and merged back in collection order, so results and maxMatches truncation match a sequential scan.
- Compiled wildcard patterns are cached in a process-wide map, mirroring the search cache; `*` and simple prefix/suffix globs short-circuit without the cache.
- Directory traversal for count-files and search skips symlinks and Windows junctions so reparse-point cycles cannot cause unbounded recursion, and reuses the directory-listing file type instead of per-entry metadata calls.
## Web Architecture

web_tools and core::web implement a two-tier fetch design: a native TIER-1 path for static/API content, and an external-CLI TIER-2 path for JavaScript-rendered content.

TIER-1 (web-fetch, download-to-file, and file-read isUrl):

- ureq is a blocking HTTP/1.1 client with no async runtime; TLS is rustls.
- http_fetch owns a single redirect loop: max_redirects(0) and http_status_as_error(false) on the ureq agent return every 3xx to the caller, so each hop is re-validated by the SSRF guard before being followed instead of trusting ureq's own redirect handling.
- Agents are cached per (scheme | host:port | validated IP set) key with a 32-entry FIFO cap, so repeated requests to the same host reuse TCP/TLS sessions; the per-request timeout is injected through request-level config, and a changed DNS answer changes the key.
- One wall-clock deadline spans the whole redirect chain (not one timeout per hop).
- The response body is read through ureq's .limit(), and the effective cap is min(caller maxBytes, MAX_ALLOWED_BYTES) (200,000,000 bytes) regardless of what the caller requests.
- download-to-file streams the body into a temp file in the target directory and renames it into place, so partial downloads never land on the target path and large bodies are not buffered in memory.

SSRF guard (ensure_url_allowed / is_allowed_ip in core::web):

- Only http:// and https:// schemes are accepted.
- The host is resolved to concrete IP addresses; every resolved address must be public.
- IPv4: private, link-local, broadcast, documentation, unspecified, CGNAT (100.64.0.0/10), "this network" (0.0.0.0/8), and multicast/reserved (>= 224.0.0.0/4) are rejected. Loopback (127.0.0.0/8) is allowed so local development servers can be reached.
- IPv6: unspecified, multicast, unique-local (fc00::/7), and link-local (fe80::/10) are rejected, while loopback (::1) is allowed. Addresses that embed an IPv4 target - IPv4-mapped (::ffff:a.b.c.d), the deprecated IPv4-compatible form (::a.b.c.d), NAT64 (64:ff9b::/96), and 6to4 (2002::/16) - are canonicalized to the embedded IPv4 address and re-checked, so they cannot smuggle a private target past the guard; embedded-IPv4 loopback forms are not treated as the loopback exception and stay blocked.
- The guard runs before every hop of a redirect, not only on the original URL.
- The addresses validated by the guard are pinned into the agent's resolver, so the connection targets exactly the checked IPs and a DNS answer that changes between check and connect (DNS rebinding) cannot bypass the guard.
- The guard always stays enabled.
- Residual accepted risk: the guard checks the resolved address at request time; a DNS answer that changes between the check and the TCP connect (DNS rebinding) is not defended against.

TIER-2 (web-render):

- Delegates navigation and DNS to an external obscura(-like) headless-browser CLI through core::external::ExternalTool::Obscura, resolved from PATH.
- The url argument still passes through ensure_url_allowed before the process is spawned.
- evalScript is rejected because in-browser requests can bypass the URL-level guard.

HTML extraction (web-extract, and the non-html dump modes of web-fetch/web-render):

- html2text renders plain wrapped text.
- htmd renders markdown.
- scraper extracts and deduplicates links, resolving relative href values against the page URL.
- dom_smoothie extracts Readability-style main-content (title, byline, text, content HTML).

download-to-file writes the fetched body to the resolved target path, with its own default body cap (50,000,000 bytes) separate from web-fetch's (5,000,000 bytes); both are clamped to the same 200,000,000-byte hard ceiling.

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
