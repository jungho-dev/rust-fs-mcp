# FS.md

## Purpose

1. **Scope**
    - This is the agent-neutral routing guide for any MCP host that loads `rust-fs-mcp`, such as Claude Code or Codex.
    - A host's own instruction file may specialize it (native edit tool, output caps, hooks); this file covers only the routes the server provides.
2. **Preferred routes**
    - Use `rust-fs-mcp` first for local reads, directory listing, metadata, structured search, git work, and URL fetch.
    - Use the host's native edit tool for targeted manual edits and deliberate full rewrites, such as Claude Code `Edit`/`Write` or Codex `apply_patch`.
    - Use `rust-fs-mcp` write tools for generated, batched, line-addressed, or structured mutations where they materially reduce tool calls or preserve a machine contract.
    - Built-in read, search, glob, and list tools are fallback only when the matching `rust-fs-mcp` call errors or lacks the capability.
3. **Web routes**
    - Prefer `web-fetch` for raw or batched URL retrieval, HTML extraction, and downloads.
    - Keep the host's search tool for search-engine queries and its page-answer tool for a page-specific answer when that route is sufficient; `rust-fs-mcp` does not query search engines.
4. **Shell and naming**
    - Use shell only for build, test, runtime, package, network, or CLI behavior outside these routes.
    - Tool names are hyphenated (`file-read`); hosts qualify them by their own rule, such as Claude Code `mcp__rust-fs-mcp__file-read` and Codex `mcp__rust_fs_mcp__file_read`.
    - The routing list below uses the short hyphenated names.

---

## Routing

1. **Read**
    - File read, single or many: `file-read` with `paths[]` or `items[]`.
    - Line-range read: `file-read-line-range` with `start_line` and `line_count`.
    - Metadata or existence probe: `path-stat`, or `file-read` with `allowMissing: true`.
2. **Edit and write**
    - Targeted manual edit or deliberate full rewrite: the host's native edit tool.
    - Generated or batched write: `file-write`.
    - Batched exact replacement: `file-edit`.
    - Known 1-based line edit: `file-edit-lines`.
3. **Paths and directories**
    - Copy, move, or remove: `path-copy`, `path-move`, `path-remove`.
    - Directory list or create: `dir-list`, `dir-create`.
4. **Search and git**
    - Text or regex search, single or batched, including code identifiers: `fs-search`.
    - When the host also provides a code graph, structural questions (symbols, callers, dependencies) go to the graph first; `fs-search` then serves text checks.
    - Git work: `git-status`, `git-diff`, `git-show`, `git-add`, `git-commit`, `git-amend`.
5. **Web**
    - URL fetch, single or batch: `web-fetch`.
    - Held HTML to text/markdown/links/readability: `web-extract`.
    - Download URL to file: `download-to-file`.
6. **Large payloads**
    - Large read-only arguments: `*_path` or `args_path` indirection.
    - Large write or edit payloads: inline target plus `content_path`, `old_string_path`, `new_string_path`, or `replacement_path`.

---

## Fallback

1. **Conditions**
    - Fall back only when the corrected preferred call still errors or the capability is absent.
    - State the fallback reason briefly, then return to the preferred route for later file work.
    - Use shell git only when the matching MCP git operation is unavailable.

---

## Rules

1. **Paths and batching**
    - Use absolute paths; relative paths resolve against the server working directory.
    - Batch two or more same-kind operations into one `items[]` or `paths[]` call.
    - Keep read batches near 3-8 files per call and split larger sets.
    - For git status/diff, directory listing or probing, and multi-file existence probes, prefer `rust-fs-mcp` even for a single operation.
2. **Read size**
    - Read a large file by the needed range only: locate the span with `fs-search` first, then read it with `file-read-line-range`.
    - Oversized results are truncated per item, never rejected; a truncated item ends with `next: offset=N` or `next: start_line=N`, so continue from there instead of rereading.
    - Use `allowMissing: true` when probing possibly missing paths.
3. **Write targets**
    - Keep every write or edit target inline.
    - Reserve top-level `args_path` for read-only calls, because a write target hidden in a file cannot be checked by host-side guards.
4. **Sources and git**
    - Prefer source files over generated, cached, vendor, build, log, backup, or temp artifacts.
    - Commit only when the user asks.

---

## Known Cost

1. **Git**
    - Multi-revision `git-show` is efficient with `objects[]` and `stat: true`; use `git-diff` with `nameOnly` for changed-file lists.
2. **Web**
    - `rust-fs-mcp` has no JavaScript-rendering tool; client-rendered (SPA) pages cannot be fetched through it.
