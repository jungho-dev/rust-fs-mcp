# rust-fs-mcp Routing Guide for Gemini CLI

## Purpose

1. **Scope**
    - The routing guide Gemini CLI loads so the agent prefers `rust-fs-mcp` tools over its built-in tools for file, search, git, and web work.
    - It covers only the routes the server provides; build, test, and runtime work stays on the shell.
    - The Gemini CLI site states that Antigravity CLI replaced Gemini CLI for unpaid tier and Google One users on June 18th, 2026; this guide covers Gemini CLI only.
2. **Tool names**
    - Server tool names are hyphenated (`file-read`), and this guide uses those short names.
    - Gemini CLI gives every MCP tool the fully qualified name `mcp_{serverName}_{toolName}`, so a server registered as `rust-fs-mcp` exposes `mcp_rust-fs-mcp_file-read`.
    - Gemini CLI keeps letters, digits, `_`, `-`, `.`, and `:` in tool names, so the hyphens stay.
    - Keep `_` out of the server name: the policy parser splits a qualified name on the first `_` after `mcp_`.
3. **Supported hosts**
    - `rust-fs-mcp` supports only five MCP hosts: Claude Code, OpenAI Codex, GitHub Copilot (VS Code agent mode and Copilot CLI), Gemini CLI, and OpenCode.
    - Each host has its own guide under `docs/<agent>/FS.md`; other MCP clients may connect but are not supported.

---

## Setup

### 1. Register With The CLI

1. **gemini mcp add**
    - `gemini mcp add <name> <commandOrUrl>` writes the server into `settings.json`; the server takes no arguments, so the command is the absolute binary path.
    - `-s user` writes `~/.gemini/settings.json`; the default `project` scope writes `.gemini/settings.json` in the project root.

```bash
gemini mcp add -s user rust-fs-mcp /absolute/path/to/rust-fs-mcp
```

### 2. Register With settings.json

1. **Settings file**
    - Gemini CLI reads servers from the top-level `mcpServers` object in `~/.gemini/settings.json` (user) or `.gemini/settings.json` (project).
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
    - Gemini CLI loads `~/.gemini/GEMINI.md` globally, then `GEMINI.md` files in the workspace directories and their parents.
    - Copy this file next to `GEMINI.md` and import it with an `@./FS.md` line; imports accept relative and absolute paths.
    - `context.fileName` in `settings.json` changes or extends the instruction file names.
    - `/memory show` prints the combined context to confirm the import loaded.

```markdown
# GEMINI.md

@./FS.md
```

---

## Native Tools

1. **Replaced by rust-fs-mcp**
    - `read_file`: `file-read` for whole files, `file-read-line-range` for line ranges.
    - `read_many_files`: `file-read` with `paths[]`.
    - `grep_search` (legacy alias `search_file_content`): `fs-search`.
    - `list_directory`: `dir-list`.
    - `glob`: `path-find`.
    - `run_shell_command` running `cat`, `ls`, `grep`, or `git` for file and git work: the matching read, list, search, or `git-*` tool.
    - `run_shell_command` running `fd`, `sd`, or `diff` for file work: `path-find`, `file-replace`, or `file-diff`.
2. **Native stays preferred**
    - `replace` for targeted manual edits.
    - `write_file` for deliberate file creation or full rewrites.
    - `google_web_search` for search-engine queries, because `rust-fs-mcp` does not query search engines.
    - `web_fetch` when its processed answer for a URL is enough; `web-fetch` for raw or batched retrieval.
    - `run_shell_command` for build, test, runtime, package, network, or CLI behavior outside these routes.
3. **Native as fallback**
    - `read_file`, `read_many_files`, `grep_search`, and `list_directory` only when the matching `rust-fs-mcp` call errors or lacks the capability.
    - `glob` for name-pattern file discovery only when `path-find` errors.

---

## Routing

1. **Read**
    - File read, single or many: `file-read` with `paths[]` or `items[]`.
    - Line-range read: `file-read-line-range` with `start_line` and `line_count`.
    - Metadata or existence probe: `path-stat`, or `file-read` with `allowMissing: true`.
2. **Edit and write**
    - Targeted manual edit: `replace`.
    - Deliberate file creation or full rewrite: `write_file`.
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
8. **Large payloads**
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

1. **Server options**
    - `timeout` sets the request timeout in milliseconds (default 600,000, 10 minutes).
    - `trust: true` bypasses every tool call confirmation for the server (default `false`).
    - `includeTools` limits the server to the listed tools, and `excludeTools` removes tools; `excludeTools` wins when a tool is in both.
2. **Global rules**
    - The `mcp` object in `settings.json` takes `allowed` and `excluded` server-name lists for all MCP servers.
3. **Name length**
    - Gemini CLI truncates tool names longer than 63 characters; every `mcp_rust-fs-mcp_<tool>` name stays under that.
4. **Result shape**
    - Gemini CLI passes only `content` to the model, so the server matches the `initialize` client name `gemini-cli-mcp-client` (recorded from Gemini CLI 0.67.0) and returns one plain `content` text block with the full body and no `structuredContent`.
    - `gemini mcp list` checks servers with a separate test client, `mcp-test-client`, which gets the default envelope; only the status check sees that.
    - The server's `instructions` name `read_file`, `read_many_files`, `grep_search`, `list_directory`, `replace`, `write_file`, `google_web_search`, and `web_fetch` as the native routes to replace or keep.
    - An explicit `compress` in the server's config.toml overrides this default.

---

## Known Cost

1. **Git**
    - Multi-revision `git-show` is efficient with `objects[]` and `stat: true`.
    - Use `git-diff` with `nameOnly` for changed-file lists.
2. **Web**
    - `rust-fs-mcp` has no JavaScript-rendering tool, so client-rendered (SPA) pages cannot be fetched through it.
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

---

## Host Docs

1. **Gemini CLI**
    - MCP servers, naming, and `gemini mcp add`: <https://geminicli.com/docs/tools/mcp-server/>
    - `GEMINI.md` context files and imports: <https://geminicli.com/docs/cli/gemini-md/>
    - Built-in tool names: <https://geminicli.com/docs/reference/tools/>
    - Settings file locations: <https://geminicli.com/docs/reference/configuration/>
