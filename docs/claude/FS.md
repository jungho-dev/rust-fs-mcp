# rust-fs-mcp Routing Guide for Claude Code

## Purpose

1. **Scope**
    - The routing guide Claude Code loads so the agent prefers `rust-fs-mcp` tools over its built-in tools for file, search, git, and web work.
    - It covers only the routes the server provides; build, test, and runtime work stays on the shell.
2. **Tool names**
    - Server tool names are hyphenated (`file-read`), and this guide uses those short names.
    - Claude Code calls an MCP tool as `mcp__<server>__<tool>`, so a server registered as `rust-fs-mcp` exposes `mcp__rust-fs-mcp__file-read`.
    - Claude Code turns characters such as `.` or spaces in a server name into `_`; `rust-fs-mcp` keeps its hyphens.
3. **Supported hosts**
    - `rust-fs-mcp` supports only five MCP hosts: Claude Code, OpenAI Codex, GitHub Copilot (VS Code agent mode and Copilot CLI), Gemini CLI, and OpenCode.
    - Each host has its own guide under `docs/<agent>/FS.md`; other MCP clients may connect but are not supported.

---

## Setup

### 1. Register With The CLI

1. **claude mcp add**
    - Everything after `--` is the server command; the server takes no arguments, so the command is the absolute binary path.
    - `--scope` picks where the entry lives: `local` (default, current project, `~/.claude.json`), `project` (shared `.mcp.json` at the project root), or `user` (every project, `~/.claude.json`).

```bash
claude mcp add --scope user rust-fs-mcp -- /absolute/path/to/rust-fs-mcp
```

### 2. Register With .mcp.json

1. **Project file**
    - A project-scoped server sits under `mcpServers` in `.mcp.json` at the project root, and Claude Code waits for your approval before it starts a server from that file.
    - On Windows, point `command` to `rust-fs-mcp.exe` and use forward slashes or escaped backslashes.

```json
{
  "mcpServers": {
    "rust-fs-mcp": {
      "command": "/absolute/path/to/rust-fs-mcp"
    }
  }
}
```

### 3. Load This Guide

1. **Instruction file**
    - Claude Code reads the project `CLAUDE.md` and the user file `~/.claude/CLAUDE.md`.
    - Copy this file next to `CLAUDE.md` and import it with an `@FS.md` line, or paste its sections into `CLAUDE.md` directly.
    - A relative import path resolves from the file that holds the import, not from the working directory.
    - A project `CLAUDE.md` import that resolves outside the working directory shows a one-time approval dialog; imports in `~/.claude/CLAUDE.md` load without it.

```markdown
# CLAUDE.md

@FS.md
```

---

## Native Tools

1. **Replaced by rust-fs-mcp**
    - `Read`: `file-read` for whole files and batches, `file-read-line-range` for line ranges.
    - `Grep`: `fs-search`.
    - `Glob`: `path-find` for name, glob, extension, size, or modification-time discovery, `dir-list` with `depth` for directory listing, and `path-stat` for existence probes.
    - `Bash` running `git status`, `git diff`, `git show`, `git add`, or `git commit`: the matching `git-*` tool.
    - `Bash` running `cat`, `ls`, `find`, or `grep` for file work: the matching read, list, find, or search tool.
    - `Bash` running `fd`, `sd`, `diff`, or `ast-grep` for file work: `path-find`, `file-replace`, `file-diff`, or `ast-search`.
    - On macOS, Linux, and WSL, Claude Code leaves `Grep` and `Glob` out of the default tool set and searches with `find` and `grep` through `Bash`; route those searches to `fs-search`, `path-find`, and `dir-list` as well.
2. **Native stays preferred**
    - `Edit` for targeted manual edits.
    - `Write` for deliberate file creation or full rewrites.
    - `NotebookEdit` for Jupyter notebook cells.
    - `WebSearch` for search-engine queries, because `rust-fs-mcp` does not query search engines.
    - `WebFetch` when a page-specific answer is enough.
    - `Bash` or `PowerShell` for build, test, runtime, package, network, or CLI behavior outside these routes.
3. **Native as fallback**
    - `Read` and `Grep` only when the matching `rust-fs-mcp` call errors or lacks the capability.
    - `Glob` for name-pattern discovery only when `path-find` errors.

---

## Routing

1. **Read**
    - File read, single or many: `file-read` with `paths[]` or `items[]`.
    - Line-range read: `file-read-line-range` with `start_line` and `line_count`.
    - Metadata or existence probe: `path-stat`, or `file-read` with `allowMissing: true`.
2. **Edit and write**
    - Targeted manual edit: `Edit`.
    - Deliberate file creation or full rewrite: `Write`.
    - Generated or batched write: `file-write`.
    - Batched exact replacement: `file-edit`.
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
    - When a code graph or `LSP` is available, structural questions (symbols, callers, dependencies) go there first, and `fs-search` serves text checks.
    - Structural code search by syntax pattern (`$VAR`, `$$$ARGS`) or a rewrite preview: `ast-search`; apply the rewrite with `file-replace` `mode: "ast"`.
5. **Git**
    - `git-status`, `git-diff`, `git-show`, `git-add`, `git-commit`, `git-amend`.
    - Two local files, or a file against inline text, without git: `file-diff`; `git-diff` stays the route for repository revisions.
6. **Web**
    - URL fetch, single or batch: `web-fetch`.
    - Held HTML to text, markdown, links, or readability: `web-extract`.
    - URL download to a file: `download-to-file`.
7. **Large payloads**
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
    - Reserve top-level `args_path` for read-only calls, because a write target hidden in a file cannot be checked by hooks or permission rules.
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

1. **Output size**
    - Claude Code limits MCP output to 25,000 tokens by default; `MAX_MCP_OUTPUT_TOKENS` changes that limit for tools that do not declare their own.
    - Without a declared limit, Claude Code saves a successful text result longer than 50,000 characters to a file and leaves only the file path in the conversation.
    - The server declares `_meta` `anthropic/maxResultSizeChars` on every tool from its output budget, capped at the 500,000-character ceiling Claude Code accepts, so results stay inline.
    - Image results stay subject to `MAX_MCP_OUTPUT_TOKENS`.
2. **Tool search**
    - Claude Code defers MCP tool definitions by default and loads them on demand through `ToolSearch`.
    - The server marks `file-read`, `file-read-line-range`, `fs-search`, `path-find`, `file-edit`, `file-edit-lines`, `dir-list`, `path-stat`, `git-status`, `git-diff`, and `git-show` with `_meta` `anthropic/alwaysLoad: true`, so those load upfront.
3. **Permissions**
    - An allow rule `mcp__rust-fs-mcp__*` (or `mcp__rust-fs-mcp`) in `permissions.allow` approves every tool from this server.
    - One tool takes its full name, such as `mcp__rust-fs-mcp__file-read`.
4. **Result shape**
    - The server matches the `initialize` client name `claude-code` (recorded from Claude Code 2.1.318) and keeps the default envelope: `content` carries a header line and the body sits in `structuredContent`.
    - Claude Code passes the serialized `structuredContent` to the model, so the body arrives intact.
    - `compress = "lossless"` in the server's config.toml switches every result to one plain `content` text block.

---

## Known Cost

1. **Git**
    - Multi-revision `git-show` is efficient with `objects[]` and `stat: true`.
    - Use `git-diff` with `nameOnly` for changed-file lists.
2. **Web**
    - `rust-fs-mcp` has no JavaScript-rendering tool, so client-rendered (SPA) pages cannot be fetched through it.
3. **Structural search**
    - `ast-search` and `file-replace` `mode: "ast"` need the `ast-grep` CLI; without it they return an install-guidance error, and every other tool keeps working.
    - The server looks for `ast-grep` in `RUST_FS_MCP_AST_GREP` (absolute, or relative to the server working directory), then on Windows in the server executable folder, `System32`, and the Windows folder, then in absolute `PATH` entries, and last in the native executable of an npm `@ast-grep/cli` install beside its `ast-grep.cmd` shim.
    - A `.cmd`, `.bat`, or `.ps1` wrapper set in `RUST_FS_MCP_AST_GREP` resolves to the npm native executable or errors, and `sg` is never used as a fallback.
    - `ast-grep` parses UTF-8 only, so `ast-search` skips EUC-KR and UTF-16 files, and `mode: "ast"` accepts only UTF-8 files without BOM.
    - An `ast-search` timeout returns the matches found so far with `timedOut: true`, and errors only when nothing was collected.
4. **fancy-regex**
    - `fs-search` and `file-replace` regex mode switch to fancy-regex for look-around and backreferences, and each such item runs in a kill-able worker process that adds about 13 ms for the spawn; other calls never spawn it.
    - The worker stops at `timeout_ms` (`file-replace`, default 25000) or `timeout_ms` + 1 s (`fs-search`); a kill or worker crash fails that item, nothing is written, and the server keeps running.
    - An invalid `fs-search` regex, including a fancy-regex syntax error, falls back to a literal search; `file-replace` errors instead.
5. **path-find dates**
    - `changedWithin` and `changedBefore` dates without an offset are local time like `fd`; `TZ` is honored, and an IANA name in `TZ` needs ICU on Windows 10 1703+.
    - `details` still prints modification times in UTC RFC 3339.

---

## Host Docs

1. **Claude Code**
    - MCP setup, scopes, output limits, and tool search: <https://code.claude.com/docs/en/mcp>
    - `CLAUDE.md` and imports: <https://code.claude.com/docs/en/memory>
    - Built-in tool names: <https://code.claude.com/docs/en/tools-reference>
    - Permission rules for MCP tools: <https://code.claude.com/docs/en/permissions>
