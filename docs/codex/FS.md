# rust-fs-mcp Routing Guide for Codex

## Purpose

1. **Scope**
    - The routing guide Codex loads so the agent prefers `rust-fs-mcp` tools over shell commands for file, search, git, and web work.
    - It covers only the routes the server provides; build, test, and runtime work stays on the shell.
2. **Tool names**
    - Server tool names are hyphenated (`file-read`), and this guide uses those short names.
    - The Codex source (`codex-rs/codex-mcp`) replaces every character outside `A-Z`, `a-z`, `0-9`, and `_` with `_` in model-visible MCP names and places the tools under an `mcp__<server>__` namespace.
    - A server registered as `rust-fs-mcp` therefore exposes `file-read` as `mcp__rust_fs_mcp__file_read`.
    - The Codex docs do not state this format; check the names Codex shows for the server if a build omits the `mcp__` prefix.
3. **Supported hosts**
    - `rust-fs-mcp` supports only five MCP hosts: Claude Code, OpenAI Codex, GitHub Copilot (VS Code agent mode and Copilot CLI), Gemini CLI, and OpenCode.
    - Each host has its own guide under `docs/<agent>/FS.md`; other MCP clients may connect but are not supported.

---

## Setup

### 1. Register With The CLI

1. **codex mcp add**
    - Everything after `--` is the stdio server command; the server takes no arguments, so the command is the absolute binary path.
    - `codex mcp list` shows the configured servers.

```bash
codex mcp add rust-fs-mcp -- /absolute/path/to/rust-fs-mcp
```

### 2. Register With config.toml

1. **Config file**
    - Codex reads MCP servers from `[mcp_servers.<server-name>]` tables in `~/.codex/config.toml`, or in `.codex/config.toml` for a trusted project.
    - The ChatGPT desktop app, Codex CLI, and IDE extension share this configuration.
    - On Windows, point `command` to `rust-fs-mcp.exe` and use forward slashes.

```toml
[mcp_servers.rust-fs-mcp]
command = "/absolute/path/to/rust-fs-mcp"
```

### 3. Load This Guide

1. **Instruction file**
    - Codex reads `AGENTS.md` before doing any work: `~/.codex/AGENTS.md` (or `AGENTS.override.md`) globally, then one file per directory from the project root down to the working directory.
    - Codex documents no import syntax for `AGENTS.md`, so paste the sections of this file into `AGENTS.md` instead of linking to it.
    - Codex includes at most one instruction file per directory, so listing `FS.md` in `project_doc_fallback_filenames` does not load it next to an existing `AGENTS.md`.
    - Combined instruction files stop at `project_doc_max_bytes` (32 KiB by default).

---

## Native Tools

1. **Replaced by rust-fs-mcp**
    - Shell reads through `exec_command` (`cat`, `head`, `sed -n`): `file-read` or `file-read-line-range`.
    - Shell listing and search (`ls`, `rg`, `grep`): `dir-list`, `path-stat`, or `fs-search`.
    - Shell name discovery (`find`, `fd`): `path-find`.
    - Shell replace and compare (`sd`, `diff`): `file-replace` or `file-diff`.
    - Shell git (`git status`, `git diff`, `git show`, `git add`, `git commit`): the matching `git-*` tool.
    - Shell downloads (`curl`, `wget`) for a URL body or file: `web-fetch` or `download-to-file`.
    - Shell file metadata and process checks through `exec_command` (`wc -l`, `file`, `sha256sum`, `tasklist`): `path-stat` or `proc-list`.
    - Shell `sqlite3` through `exec_command`: `sqlite-query`.
2. **Native stays preferred**
    - `apply_patch` for targeted manual edits and deliberate full rewrites.
    - `web_search` for search-engine queries, because `rust-fs-mcp` does not query search engines.
    - `view_image` for opening local images.
    - `exec_command` for build, test, runtime, package, network, or CLI behavior outside these routes.
3. **Native as fallback**
    - Shell reads, listing, and search through `exec_command` only when the matching `rust-fs-mcp` call errors or lacks the capability.

---

## Routing

1. **Read**
    - File read, single or many: `file-read` with `paths[]` or `items[]`.
    - Line-range read: `file-read-line-range` with `start_line` and `line_count`; the last N lines: `tail_lines`, which returns real line numbers plus `totalLines` and cannot be combined with `start_line` or `line_count`.
    - Image or PDF: `file-read`; png, jpg, gif, and webp return as image blocks, avif, heic, heif, bmp, tif, tiff, ico, and jxl are converted to webp through `magick` (fit within 2000x2000), a png, jpg, gif, or webp over 5 MB is shrunk the same way, and a PDF returns its `pdftotext` text, with `items[].pages` (`"N"` or `"N-M"`) picking pages.
    - Line count, CRLF and LF counts, final newline, encoding, or SHA-256 of a file: `path-stat` with `lines: true` or `hash: "sha256"`, which also marks a binary file with `binary: true`; across a tree (`find | xargs wc -l`), `path-find` with `lines: true` or `sortBy: "lines"`.
    - Metadata or existence probe: `path-stat`, or `file-read` with `allowMissing: true`.
2. **Edit and write**
    - Targeted manual edit or deliberate full rewrite: `apply_patch`.
    - Generated or batched write: `file-write`, with no prior read.
    - Batched exact replacement: `file-edit`, with no prior read; `replace_all: true` replaces every match, and a not-found error ends with a hint (`new_string is already present at line N`, `lines A-B match except for whitespace`, `closest line N: <line>`, or `no similar line found`).
    - Known 1-based line edit: `file-edit-lines`.
    - Regex or literal replace across files, capture groups (`$1`, `${name}`), or a batch replace that keeps the file encoding: `file-replace`.
    - `file-replace` validates every item before writing, so one failing item writes nothing; `expectedReplacements` guards the match count.
    - `file-replace` matches case-sensitively unless `ignoreCase: true`, unlike the smart-case `fs-search`.
    - Preview before writing: `dryRun: true` on `file-edit`, `file-edit-lines`, or `file-replace` returns the unified diff and writes nothing.
3. **Paths and directories**
    - Copy, move, or remove: `path-copy`, `path-move`, `path-remove`.
    - Directory list or create: `dir-list`, `dir-create`.
    - Name or metadata discovery (name regex or glob, extension, size, modification time, type): `path-find`.
4. **Search**
    - Text or regex search, single or batched, including code identifiers: `fs-search`.
    - `fs-search` treats CRLF and LF files alike: `^` and `$` match at line edges in both, and a line-by-line search hides each line's terminating CR, so match `\r` only with `multiline: true`.
    - When a code graph is available, structural questions (symbols, callers, dependencies) go to the graph first, and `fs-search` serves text checks.
    - Structural rewrite by syntax pattern (`$VAR`, `$$$ARGS`): `file-replace` `mode: "ast"`, previewed with `dryRun: true`.
5. **Git**
    - `git-status`, `git-diff`, `git-show`, `git-add`, `git-commit`, `git-amend`.
    - Two local files, or a file against inline text, without git: `file-diff`; `git-diff` stays the route for repository revisions.
6. **Web**
    - URL fetch, single or batch: `web-fetch`.
    - Held HTML to text, markdown, links, or readability: `web-extract`.
    - URL download to a file: `download-to-file`.
7. **Database**
    - Read-only MySQL query, one statement per call: `mysql-query`; the tool appears only when the server's config.toml has a `[mysql]` section with `host` and `user`.
    - Read the schema first (`SHOW`, `DESCRIBE`, `information_schema` filtered by `TABLE_SCHEMA` and `TABLE_NAME`) before any `SELECT`, and put a small `LIMIT` on every `SELECT`.
    - Local SQLite file or `:memory:`, any SQL (read, write, delete, DDL, `PRAGMA`, multi-statement scripts): `sqlite-query` with `path` and `sql` or `sql_path`; it is always listed, and writes apply at once with no statement gate.
    - `readOnly: true` for read-only work and for a database another process writes, `transaction: true` for all-or-nothing writes, `create: true` for a new file, `params` or `namedParams` instead of quoting; each call opens its own connection, so `BEGIN` and `COMMIT` sit in one call; read the schema through `sqlite_schema` and `PRAGMA` (no extensions or dot-commands).
    - Redis commands on a config.toml target: `redis-command` with `commands` as 1 to 100 argv arrays and `target` when several are set; it appears only when config.toml has a `[redis]` or `[redis.<name>]` section with `host`, and no call reaches another host or port.
    - Writes and deletes apply at once, so read first; use `SCAN` with `scanAll: true` instead of `KEYS` and `db` instead of `SELECT`; Lua scripting, `SUBSCRIBE`/`MONITOR`, `AUTH`/`HELLO`, and server-changing commands are rejected.
8. **Processes**
    - Windows process list, with an optional case-insensitive `name` glob and exact `pids`: `proc-list`; it is listed on Windows only, and no match returns `0 processes`, not an error.
9. **Large payloads**
    - Large read-only arguments: `*_path` or `args_path` indirection.
    - Large write or edit payloads: an inline target plus `content_path`, `old_string_path`, `new_string_path`, `pattern_path`, or `replacement_path`.

---

## Rules

1. **Paths and batching**
    - Use absolute paths; relative paths resolve against the server working directory.
    - Batch two or more same-kind operations into one `items[]` or `paths[]` call.
    - Keep read batches near 3-8 files per call and split larger sets.
    - Prefer `rust-fs-mcp` even for a single operation for git status and diff, directory listing or probing, path discovery, and multi-file existence probes.
2. **Read size**
    - Read a large file by the needed range only: locate the span with `fs-search`, then read it with `file-read-line-range`.
    - Oversized results are truncated per item, never rejected.
    - A truncated item ends with `next: offset=N` or `next: start_line=N`; continue from there instead of rereading.
    - Use `allowMissing: true` when probing possibly missing paths.
3. **Write targets**
    - Keep every write or edit target inline, including each `file-replace` item's `file_path`.
    - Reserve top-level `args_path` for read-only calls, because a write target hidden in a file cannot be checked by host-side guards.
4. **Sources and git**
    - Prefer source files over generated, cached, vendor, build, log, backup, or temp artifacts.
    - Commit only when the user asks.

---

## Fallback

1. **Conditions**
    - Fall back only when the corrected preferred call still errors or the capability is absent.
    - State the fallback reason briefly, then return to the preferred route for later file work.
    - Use shell git only when the matching MCP git operation is unavailable.

---

## Host Limits

1. **Timeouts**
    - `startup_timeout_sec` defaults to 10 seconds and `tool_timeout_sec` to 60 seconds per server; raise `tool_timeout_sec` for long batched searches or downloads.
2. **Approval**
    - `default_tools_approval_mode` sets the default approval for this server's tools: `auto`, `prompt`, `writes`, or `approve`.
    - `writes` prompts only for tools not marked read-only; the server marks `file-read`, `file-read-line-range`, `dir-list`, `fs-search`, `path-find`, `path-stat`, `file-diff`, `git-status`, `git-diff`, `git-show`, `web-fetch`, `web-extract`, `proc-list`, and `mysql-query` with `readOnlyHint: true`.
    - `tools.<tool>.approval_mode` overrides one tool.
3. **Tool selection and output**
    - `enabled_tools` is an allow list and `disabled_tools` a deny list applied after it.
    - `tools.<tool>.output_token_limit` sets a token budget for one tool's output.
4. **Result shape**
    - The server matches the `initialize` client name `codex-mcp-client` (recorded from Codex 0.166.0) and returns each result as one plain `content` text block with no `structuredContent`, so the body and images reach the model.
    - Codex results keep the default 30,000-byte budget; in a Codex 0.166.0 check, 47 KB and 58 KB MCP results reached the model intact, so the server budget, not Codex, cuts large results.
    - Continue a truncated item from its `next:` marker instead of widening the call.
    - The server's `instructions` name `exec_command`, `apply_patch`, and `web_search` as the native routes to replace or keep.
    - A `file-read` image arrives as an image block after the text block.
    - An explicit `compress` or `max_bytes` in the server's config.toml overrides these defaults.

---

## Known Cost

1. **Git**
    - Multi-revision `git-show` is efficient with `objects[]` and `stat: true`.
    - Use `git-diff` with `nameOnly` for changed-file lists.
2. **Web**
    - `rust-fs-mcp` has no JavaScript-rendering tool, so client-rendered (SPA) pages cannot be fetched through it.
    - `web-fetch` names a bot challenge in its error; with `fallback` (default true) it retries a bot challenge or HTTP 403, 429, or 503 through a labeled Wayback Machine snapshot.
3. **Structural rewrite**
    - `file-replace` `mode: "ast"` needs the `ast-grep` CLI; without it the mode returns an install-guidance error, and every other tool keeps working.
    - The server looks for `ast-grep` in `RUST_FS_MCP_AST_GREP` (absolute, or relative to the server working directory), then on Windows in the server executable folder, `System32`, and the Windows folder, then in absolute `PATH` entries, and last in the native executable of an npm `@ast-grep/cli` install beside its `ast-grep.cmd` shim.
    - A `.cmd`, `.bat`, or `.ps1` wrapper set in `RUST_FS_MCP_AST_GREP` resolves to the npm native executable or errors, and `sg` is never used as a fallback.
    - `ast-grep` parses UTF-8 only, so `mode: "ast"` accepts only UTF-8 files without BOM.
    - A `mode: "ast"` timeout or an `ast-grep` output over the pipe cap refuses the whole rewrite, so nothing is applied.
4. **fancy-regex**
    - `fs-search` and `file-replace` regex mode switch to fancy-regex for look-around and backreferences, and each such item runs in a kill-able worker process; with the warm standby worker that adds about 3 ms (measured 2026-11-11: 8.6 ms against 5.6 ms without look-around), and other calls never spawn it.
    - The worker stops at `timeout_ms` (`file-replace`, default 25000) or `timeout_ms` + 1 s (`fs-search`); a kill or worker crash fails that item, nothing is written, and the server keeps running.
    - An invalid `fs-search` regex, including a fancy-regex syntax error, falls back to a literal search; `file-replace` errors instead.
5. **path-find dates**
    - `changedWithin` and `changedBefore` dates without an offset are local time like `fd`; `TZ` is honored, and an IANA name in `TZ` needs ICU on Windows 10 1703+.
    - `details` still prints modification times in UTC RFC 3339.
6. **Image and PDF conversion**
    - `file-read` converts non-web images through `magick` and reads PDF text through `pdftotext`; the server finds each in `RUST_FS_MCP_MAGICK` or `RUST_FS_MCP_PDFTOTEXT` (an absolute path), then in absolute `PATH` entries.
    - Without `magick`, a png, jpg, gif, or webp over 5 MB goes out unchanged and a non-web format returns an error; without `pdftotext`, a PDF read returns an error; every other tool keeps working, and each program runs without a shell, with the file path as one argv item.
7. **Database**
    - `sqlite-query` opens its own connection per call, so `BEGIN` and `COMMIT` sit in one call; `maxRows` defaults to 1000 (cap 10000) and `timeout_ms` to 30000 (cap 600000), and there is no statement gate.
    - `readOnly: true` opens the file read-only, so SQLite refuses every write to it; `VACUUM` (including `VACUUM INTO`, which would write a new file) is rejected before it runs, while TEMP tables and query-form `PRAGMA`s still work.
    - `redis-command` runs calls one at a time under one global lock.
    - A Redis reply past 64 MiB stops the batch, closes the connection, and leaves later commands unsent.

---

## Host Docs

1. **Codex**
    - MCP setup and `config.toml` server options: <https://developers.openai.com/codex/mcp>
    - `AGENTS.md` discovery: <https://developers.openai.com/codex/guides/agents-md>
    - Configuration reference: <https://developers.openai.com/codex/config-reference>
