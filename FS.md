# FS.md

## Purpose

* `rust-fs-mcp` is the required route for local file read, line read, write, edit, copy, move, remove, list, metadata,
structured search, git work, and URL fetch/crawl in Codex.
* Built-in `Read`, `Grep`, `Glob`, and `LS` tools are fallback only when the matching `rust-fs-mcp` call errors or lacks the capability.
* Prefer `rust-fs-mcp` `web-fetch`/`web-render` over the built-in `WebFetch` for raw or batched URL retrieval, HTML-to-markdown/text/links/readability extraction, and file downloads. Built-in `WebFetch`/`WebSearch` remain the route for single-page question answering and search-engine queries, which `rust-fs-mcp` does not do.
* Use shell only for build, test, runtime, package, network, or CLI behavior outside both routes.
* Tool names use underscore-separated identifiers. Codex invokes them fully-qualified as `mcp__rust_fs_mcp__<tool>`
(e.g. `mcp__rust_fs_mcp__file_read`); the Routing table below uses the short snake_case form.
* Basis: per-tool ON/OFF benchmark from 2026-06-05 showed MCP-first faster in 9/10 cases on this host and preserved
trailing-newline `lineCount` correctly.

## Routing

| Workload | Route |
|----------|-------|
| File read, single or many | `file-read` with `paths[]` or `items[]` |
| Line-range read | `file-read-line-range` with `start_line` and `line_count` |
| Write or create files | `file-write` |
| Exact block edit | `file-edit` |
| Known 1-based line edit | `file-edit-lines` |
| Copy, move, or remove | `path-copy`, `path-move`, `path-remove` |
| Directory list or create | `dir-list`, `dir-create` |
| Metadata or existence probe | `path-stat`, or `file-read` with `allowMissing: true` |
| Content or regex search | `fs-search` |
| Git work | `git-status`, `git-diff`, `git-show`, `git-add`, `git-commit`, `git-amend` |
| Bundled read-only inspection | `fs-inspect` |
| URL fetch, single or batch (HTTP/HTTPS) | `web-fetch` |
| JS-rendered / SPA page fetch | `web-render` |
| HTML already in hand -> text/markdown/links/readability | `web-extract` |
| Download a URL to a sandboxed file | `download-to-file` |
| Large generated arguments | `*_path` or `args_path` indirection |

## Fallback

* Fall back only when the corrected `rust-fs-mcp` call still errors or the capability is absent.
* Use `apply_patch` for manual edits. Use shell git only when the matching MCP git operation is unavailable.
* State the fallback reason briefly, then return to `rust-fs-mcp` for subsequent file work.

## Rules

* Use absolute paths.
* Batch two or more same-kind operations into one `items[]` or `paths[]` call.
* Keep read batches near 3-8 files per call and split larger sets; page large files with `file-read-line-range`.
* Use `allowMissing: true` when probing possibly missing paths.
* Prefer source files over generated, cached, vendor, build, log, backup, or temp artifacts.
* Commit only when the user asks.
* For git status/diff, directory listing or probing, and multi-file existence probes, prefer `rust-fs-mcp` even for a single operation; the built-in path shells out and is slower.

## Known Cost

* Multi-revision `git-show` is efficient now: pass `objects[]` with `stat: true` to fetch several revisions' diffstats
  in one call (single git invocation, no full patch). Use `git-diff` with `nameOnly` for changed-file lists.
* `web-render` shells out to a headless browser and is much heavier than `web-fetch`. Try `web-fetch` first and escalate
  to `web-render` only when the page requires JS execution.
