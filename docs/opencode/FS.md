# rust-fs-mcp Routing Guide for OpenCode

## Purpose

1. **Scope**
    - The routing guide OpenCode loads so the agent prefers `rust-fs-mcp` tools over its built-in tools for file, search, git, and web work.
    - It covers only the routes the server provides; build, test, and runtime work stays on the shell.
    - It targets OpenCode V2, the current docs version, and notes OpenCode V1 differences where they change the setup.
2. **Tool names**
    - Server tool names are hyphenated (`file-read`), and this guide uses those short names.
    - OpenCode names an MCP tool `<server>_<tool>` and replaces characters other than letters, digits, `_`, and `-` with `_`, so a server registered as `rust-fs-mcp` exposes `rust-fs-mcp_file-read`.
    - By default OpenCode reaches MCP tools through Code Mode: the agent calls them as JavaScript inside the `execute` tool, grouped under the server namespace, instead of as direct tool calls.
    - `"codemode": false` on the server entry, as in the setup below, lists its tools directly on the model's tool list.
3. **Supported hosts**
    - `rust-fs-mcp` supports only five MCP hosts: Claude Code, OpenAI Codex, GitHub Copilot (VS Code agent mode and Copilot CLI), Gemini CLI, and OpenCode.
    - Each host has its own guide under `docs/<agent>/FS.md`; other MCP clients may connect but are not supported.

---

## Setup

### 1. Register With The CLI

1. **opencode mcp add**
    - Everything after `--` is the local server command; the server takes no arguments, so the command is the absolute binary path.
    - The command writes the project configuration; `--global` writes the global configuration for every project.
    - The command does not set `codemode`, so add `"codemode": false` to the written entry afterwards.
    - `opencode mcp list` shows each server and its connection state.

```bash
opencode mcp add rust-fs-mcp --global -- /absolute/path/to/rust-fs-mcp
```

### 2. Register With opencode.json

1. **Config file**
    - OpenCode reads servers from `mcp.servers` in `opencode.json` or `opencode.jsonc`: `~/.config/opencode/` for the user, or the project root or its `.opencode/` directory for a project.
    - A local entry needs `type: "local"` and a `command` array of the executable and its arguments; the server takes no arguments, so the array holds only the absolute binary path.
    - Servers connect automatically; `disabled: true` keeps an entry configured without connecting it.
    - `"codemode": false` puts the server's tools on the direct tool list, which the routing below assumes.
    - On Windows, point `command` to `rust-fs-mcp.exe` and use forward slashes or escaped backslashes.
    - OpenCode V1 places servers directly under `mcp` and uses `enabled` instead of `disabled`; V2 still accepts that shape.

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "rust-fs-mcp": {
        "type": "local",
        "command": ["/absolute/path/to/rust-fs-mcp"],
        "codemode": false
      }
    }
  }
}
```

### 3. Load This Guide

1. **Instruction file**
    - OpenCode loads `~/.config/opencode/AGENTS.md` globally, then every `AGENTS.md` from the workspace directory toward the home directory, stopping at the project root for a workspace outside the home directory.
    - OpenCode V2 recognizes `AGENTS.md` only and does not fall back to `CLAUDE.md`.
    - The `instructions` array in `opencode.json` adds files, globs, or URLs after the `AGENTS.md` files, so save this file and list its path there.
    - A relative entry is searched in each directory from the workspace up to the project root, a `~/` entry resolves from the home directory, and an absolute entry is used as written.
    - OpenCode documents no import syntax for `AGENTS.md`, so list this file in `instructions` or paste its sections into `AGENTS.md` instead of linking to it.

```json
{
  "$schema": "https://opencode.ai/config.json",
  "instructions": ["~/.config/opencode/FS.md"]
}
```

---

## Native Tools

1. **Replaced by rust-fs-mcp**
    - `read` on a file: `file-read` for whole files and batches, `file-read-line-range` for line ranges.
    - `read` on a directory, which returns a non-recursive listing: `dir-list`.
    - `grep`: `fs-search`.
    - `glob`: `path-find`.
    - `shell` running `cat`, `ls`, `find`, `grep`, or `git` for file and git work: the matching read, list, find, search, or `git-*` tool.
    - `shell` running `fd`, `sd`, `diff`, or `ast-grep` for file work: `path-find`, `file-replace`, `file-diff`, or `ast-search`.
    - `webfetch` for batched URLs or binary downloads, which it rejects: `web-fetch` or `download-to-file`.
2. **Native stays preferred**
    - `edit` for targeted manual edits.
    - `write` for deliberate file creation or full rewrites.
    - `patch` for multi-file patches on the GPT models that receive it in place of `edit` and `write`.
    - `websearch` for search-engine queries, because `rust-fs-mcp` does not query search engines.
    - `webfetch` when one page as markdown is enough.
    - `shell` for build, test, runtime, package, network, or CLI behavior outside these routes.
3. **Native as fallback**
    - `read` and `grep` only when the matching `rust-fs-mcp` call errors or lacks the capability.
    - `glob` for name-pattern file discovery only when `path-find` errors.
4. **OpenCode V1 names**
    - OpenCode V1 calls the shell tool `bash` and the patch tool `apply_patch`; the routes above apply to them unchanged.

---

## Routing

1. **Read**
    - File read, single or many: `file-read` with `paths[]` or `items[]`.
    - Line-range read: `file-read-line-range` with `start_line` and `line_count`.
    - Metadata or existence probe: `path-stat`, or `file-read` with `allowMissing: true`.
2. **Edit and write**
    - Targeted manual edit: `edit`.
    - Deliberate file creation or full rewrite: `write`.
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
    - When a code graph is available, structural questions (symbols, callers, dependencies) go to the graph first, and `fs-search` serves text checks.
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

1. **Output size**
    - OpenCode keeps at most 2,000 lines and 51,200 bytes of a tool result by default; `tool_output.max_lines` and `tool_output.max_bytes` change the limits.
    - The OpenCode source cuts a longer result at those limits, saves the full text to a file, and appends a marker with that file path.
    - Keep `rust-fs-mcp` batches and ranges small enough to stay under the limit, or raise it.
2. **Result shape**
    - OpenCode passes `content` to the model whenever it is non-empty, so the server matches the `initialize` client name `opencode` (recorded from OpenCode 2.2.5) and returns one plain `content` text block with the full body and no `structuredContent`.
    - Some OpenCode models send a path with its JSON quotes still inside, such as `"\"C:/work/a.txt\""`; the server unwraps a quoted absolute path.
    - The server's `instructions` name `read`, `grep`, `edit`, `write`, `websearch`, and `webfetch` as the native routes to replace or keep.
    - An explicit `compress` in the server's config.toml overrides this default.
3. **Timeouts**
    - `startup` and `catalog` default to 30 seconds and `execution` to 12 hours.
    - `mcp.timeout` sets the defaults, and the server's own `timeout` object overrides them, in milliseconds.
4. **Server instructions**
    - OpenCode adds the instructions a server returns at initialization to the system prompt.
    - It omits them when the agent's permissions deny every tool of the server, or deny `execute` for a server that uses Code Mode.
5. **Permissions**
    - A permission rule for an MCP tool uses the action `<server>_<tool>` with resource `*`, so `rust-fs-mcp_*` matches every tool from this server.
    - The default base policy allows every action, so `rust-fs-mcp` tools run without a prompt until a rule sets `ask` or `deny`.
    - The last matching rule wins, so a narrower rule follows the broad one.
    - OpenCode matches MCP tools by name only; path rules such as `external_directory` do not apply to the paths in their arguments.
    - With Code Mode on, a `deny` for `execute` also removes every MCP tool reached only through Code Mode.

```json
{
  "permissions": [
    {
      "action": "rust-fs-mcp_*",
      "resource": "*",
      "effect": "allow"
    },
    {
      "action": "rust-fs-mcp_path-remove",
      "resource": "*",
      "effect": "ask"
    }
  ]
}
```

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

1. **OpenCode**
    - MCP servers, tool names, Code Mode, timeouts, and server instructions: <https://opencode.ai/docs/mcp-servers/>
    - Built-in tool names: <https://opencode.ai/docs/tools/>
    - `AGENTS.md` and the `instructions` key: <https://opencode.ai/docs/instructions/>
    - Permission rules and defaults: <https://opencode.ai/docs/permissions/>
    - Config locations and `tool_output`: <https://opencode.ai/docs/config/>
    - V1 to V2 configuration changes: <https://opencode.ai/docs/migrate-v1/>
    - Source repository: <https://github.com/anomalyco/opencode>
