# rust-fs-mcp Routing Guide for GitHub Copilot

## Purpose

1. **Scope**
    - The routing guide GitHub Copilot in VS Code loads so the agent prefers `rust-fs-mcp` tools over its built-in tools for file, search, git, and web work.
    - It covers only the routes the server provides; build, test, and runtime work stays on the terminal.
    - VS Code runs agent sessions in a Local harness or a Copilot harness; the built-in tool names below are the Local harness reference names, and the Copilot harness lists its own tools in the **Tools** tab.
    - It also covers GitHub Copilot CLI (`copilot`): sections and items labeled CLI apply to the CLI only, and the rest describes VS Code.
2. **Tool names**
    - Server tool names are hyphenated (`file-read`), and this guide uses those short names.
    - A custom agent's `tools` list takes `<server name>/*` to include every tool a server publishes, so `rust-fs-mcp/*` covers this server.
    - Type `#` in the chat input to see the tool references the session offers.
3. **CLI tool names**
    - Copilot CLI sends an MCP tool to the model as `<server>-<tool>` and replaces characters other than `a-z`, `A-Z`, `0-9`, `-`, and `_` with `-`, so a server registered as `rust-fs-mcp` exposes `rust-fs-mcp-file-read`.
    - The CLI caps the combined name at 64 characters; every `rust-fs-mcp-<tool>` name stays under that.
4. **Supported hosts**
    - `rust-fs-mcp` supports only five MCP hosts: Claude Code, OpenAI Codex, GitHub Copilot (VS Code agent mode and Copilot CLI), Gemini CLI, and OpenCode.
    - Each host has its own guide under `docs/<agent>/FS.md`; other MCP clients may connect but are not supported.

---

## Setup

### 1. Register With .vscode/mcp.json

1. **VS Code format**
    - `.vscode/mcp.json` in the workspace, or `mcp.json` in the user profile (**MCP: Open User Configuration**), defines servers under a top-level `servers` object.
    - Each stdio entry needs `type: "stdio"` and `command`; the server takes no arguments, so `command` is the absolute binary path.
    - Copilot harness sessions do not read this file directly; VS Code forwards eligible servers from it to the session.
    - On Windows, point `command` to `rust-fs-mcp.exe` and use forward slashes or escaped backslashes.

```json
{
  "servers": {
    "rust-fs-mcp": {
      "type": "stdio",
      "command": "/absolute/path/to/rust-fs-mcp"
    }
  }
}
```

### 2. Register With The Portable Format

1. **Copilot format**
    - Workspace-root `.mcp.json`, or `~/.copilot/mcp-config.json` (`$COPILOT_HOME/mcp-config.json` when `COPILOT_HOME` is set) for the user, defines servers under a top-level `mcpServers` object.
    - Copilot harness sessions read this format directly.
    - Do not copy an entry between the two formats without checking the top-level key.
    - Copilot CLI also reads `~/.copilot/mcp-config.json` and documents more required fields for a local entry; use the CLI entry in section 5 for that file.

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
    - Copilot reads the project file `.github/copilot-instructions.md` or `AGENTS.md`, plus file-scoped `.github/instructions/**/*.instructions.md` files.
    - User-level instructions live in `~/.copilot/copilot-instructions.md` and `~/.copilot/instructions/**/*.instructions.md`.
    - Paste the sections of this file into `.github/copilot-instructions.md`, or save them as `.github/instructions/rust-fs-mcp.instructions.md` with an `applyTo: '**'` header.
    - A Markdown link to this file does not guarantee its contents reach the model, so copy the sections instead of linking.
    - In Local sessions, `github.copilot.chat.codeGeneration.useInstructionFiles` includes `.github/copilot-instructions.md` and `chat.includeApplyingInstructions` includes matching `.instructions.md` files.

```markdown
---
applyTo: '**'
---
# rust-fs-mcp Routing Guide for GitHub Copilot
```

### 4. Register With Copilot CLI

1. **CLI command**
    - `copilot mcp add <name> -- <command>` adds a local stdio server to the user configuration `~/.copilot/mcp-config.json`; the server takes no arguments, so the command is the absolute binary path.
    - `--tools` filters the server's tools (`*` by default), and `--timeout` sets the discovery and tool-call timeout in milliseconds (default `30000`).
    - `/mcp add` in an interactive session fills the same configuration through a form, and the server is available without a restart.
    - `copilot mcp list` and `copilot mcp get rust-fs-mcp` show the server, its status, and its tools.

```bash
copilot mcp add rust-fs-mcp -- /absolute/path/to/rust-fs-mcp
```

### 5. Register With mcp-config.json

1. **CLI config file**
    - Copilot CLI reads user servers from the top-level `mcpServers` object in `~/.copilot/mcp-config.json`; `COPILOT_HOME` replaces the whole `~/.copilot` directory, so the file moves to `$COPILOT_HOME/mcp-config.json`.
    - A local entry takes `type` (`"local"` or `"stdio"`), `command`, `args`, and `tools`; the CLI reference marks `command`, `args`, and `tools` as required, so pass an empty `args` array and `["*"]` for every tool.
    - Workspace `.mcp.json` files from the working directory up to the Git root, and `.github/mcp.json`, load once the folder is trusted and take precedence over the user file for the same server name.
    - The CLI does not read `.vscode/mcp.json`, because it does not support that file's `servers` key.
    - `--additional-mcp-config` adds a server for one session only.
    - On Windows, point `command` to `rust-fs-mcp.exe` and use forward slashes or escaped backslashes.

```json
{
  "mcpServers": {
    "rust-fs-mcp": {
      "type": "local",
      "command": "/absolute/path/to/rust-fs-mcp",
      "args": [],
      "tools": ["*"]
    }
  }
}
```

### 6. Load This Guide In Copilot CLI

1. **CLI instruction files**
    - Copilot CLI merges `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `.github/copilot-instructions.md`, `.github/instructions/**/*.instructions.md`, and `.claude/rules/**/*.md` from the Git root and the working directory.
    - It also reads `$HOME/.copilot/copilot-instructions.md`, `$HOME/.copilot/instructions/**/*.instructions.md`, and the directories listed in `COPILOT_CUSTOM_INSTRUCTIONS_DIRS`.
    - `AGENTS.md`, `CLAUDE.md`, and `.github/copilot-instructions.md` accept `@path` import lines, relative to the instruction file or absolute, so copy this file next to one of them and import it with `@FS.md`.
    - `copilot instruction list` prints the instruction sources the CLI discovered, and `--no-custom-instructions` turns them off.

```markdown
# AGENTS.md

@FS.md
```

---

## Native Tools

1. **Replaced by rust-fs-mcp**
    - `#read/readFile`: `file-read` for whole files and batches, `file-read-line-range` for line ranges.
    - `#search/textSearch`: `fs-search`.
    - `#search/listDirectory`: `dir-list`.
    - `#search/fileSearch`: `path-find`.
    - `#search/changes`: `git-status` and `git-diff`.
    - `#edit/createDirectory`: `dir-create`.
    - `#execute/runInTerminal` running `cat`, `ls`, `grep`, or `git` for file and git work: the matching read, list, search, or `git-*` tool.
    - `#execute/runInTerminal` running `fd`, `sd`, or `diff` for file work: `path-find`, `file-replace`, or `file-diff`.
    - `#web/fetch` for a raw or batched URL body: `web-fetch`.
2. **Native stays preferred**
    - `#edit/editFiles` for targeted manual edits and `#edit/createFile` for deliberate file creation.
    - `#edit/editNotebook` for notebooks.
    - `#search/codebase` and `#search/usages` for semantic code search and symbol references, with `fs-search` for text checks.
    - `#execute/runInTerminal` for build, test, runtime, package, network, or CLI behavior outside these routes.
3. **Native as fallback**
    - `#read/readFile`, `#search/textSearch`, and `#search/listDirectory` only when the matching `rust-fs-mcp` call errors or lacks the capability.
    - `#search/fileSearch` for glob-pattern file discovery only when `path-find` errors.
4. **CLI replaced by rust-fs-mcp**
    - `view` on a file: `file-read` for whole files and batches, `file-read-line-range` for line ranges.
    - `view` on a directory: `dir-list`.
    - `grep` (or `rg`): `fs-search`.
    - `glob`: `path-find`.
    - `bash` or `powershell` running `cat`, `ls`, `grep`, or `git` for file and git work: the matching read, list, search, or `git-*` tool.
    - `bash` or `powershell` running `fd`, `sd`, or `diff` for file work: `path-find`, `file-replace`, or `file-diff`.
    - `web_fetch` for a raw or batched URL body: `web-fetch`.
5. **CLI native stays preferred**
    - `edit` for targeted manual edits and `create` for deliberate file creation.
    - `apply_patch` on the models that use it instead of `edit` and `create`.
    - `bash` or `powershell` for build, test, runtime, package, network, or CLI behavior outside these routes.
6. **CLI native as fallback**
    - `view` and `grep` only when the matching `rust-fs-mcp` call errors or lacks the capability.
    - `glob` for pattern-based file discovery only when `path-find` errors.

---

## Routing

1. **Read**
    - File read, single or many: `file-read` with `paths[]` or `items[]`.
    - Line-range read: `file-read-line-range` with `start_line` and `line_count`.
    - Metadata or existence probe: `path-stat`, or `file-read` with `allowMissing: true`.
2. **Edit and write**
    - Targeted manual edit: `#edit/editFiles`, or `edit` in Copilot CLI.
    - Deliberate file creation in Copilot CLI: `create`.
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
    - Structural questions (symbols, callers, references) go to `#search/usages` or a code graph first, and `fs-search` serves text checks.
    - Structural rewrite by syntax pattern (`$VAR`, `$$$ARGS`): `file-replace` `mode: "ast"`, previewed with `dryRun: true`.
5. **Git**
    - `git-status`, `git-diff`, `git-show`, `git-add`, `git-commit`, `git-amend`.
    - Two local files, or a file against inline text, without git: `file-diff`; `git-diff` stays the route for repository revisions.
6. **Web**
    - URL fetch, single or batch: `web-fetch`.
    - Held HTML to text, markdown, links, or readability: `web-extract`.
    - URL download to a file: `download-to-file`.
    - `rust-fs-mcp` does not query search engines.
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
    - Use terminal git only when the matching MCP git operation is unavailable.

---

## Host Limits

1. **Tool count**
    - A chat request can have at most 128 tools enabled; this server adds 24.
    - Deselect tools or whole MCP servers in the tools picker when a request passes that limit.
2. **Approval**
    - `chat.tools.eligibleForAutoApproval` set to `false` for a tool always requires manual approval for it.
    - `chat.tools.global.autoApprove` auto-approves tools across all workspaces.
    - Enabling a tool makes it available but does not bypass approval.
3. **CLI output size**
    - Copilot CLI saves tool output larger than 20 KiB to a temporary file by default, MCP tools included, and gives the model the file path and a preview.
    - `COPILOT_LARGE_OUTPUT_THRESHOLD_BYTES` changes that limit in UTF-8 bytes; otherwise keep `rust-fs-mcp` batches and ranges small enough to stay under it.
4. **CLI approval**
    - `--allow-tool='rust-fs-mcp'` allows every tool from this server without prompting, and `--allow-tool='rust-fs-mcp(file-read)'` allows one tool.
    - `--deny-tool` takes the same patterns, and deny rules win over allow rules, even with `--allow-all` or a saved approval.
    - `--allow-tool` and `--deny-tool` apply to the current session only; an approval chosen at a prompt for the current location is saved to `permissions-config.json` in the configuration directory.
    - `--available-tools` and `--excluded-tools` limit which tools the model sees at all.
5. **CLI tool loading**
    - When tool search is active, the CLI can defer MCP tools behind search; `"deferTools": "never"` on the server entry keeps its tools visible.
    - By default the CLI puts only allowlisted servers' initialization instructions in the system prompt and retrieves other servers' instructions on demand; `--allow-all-mcp-server-instructions` includes all of them up front.
6. **Result shape**
    - The server matches the VS Code client names `Code - OSS`, `Visual Studio Code`, and `Visual Studio Code - Insiders` and returns one plain `content` text block with no `structuredContent`, because VS Code otherwise shows the serialized `structuredContent` JSON in place of the text.
    - VS Code was not launched for this guide: `Code - OSS` comes from the VS Code source, and the two official names are the documented product names.
    - The server's `instructions` name `#read/readFile`, `#search/textSearch`, `#search/listDirectory`, `#web/fetch`, `#edit/editFiles`, and `#search/codebase` as the native routes to replace or keep.
7. **CLI result shape**
    - The server matches the client name `copilot-cli` (recorded from Copilot CLI 1.0.107) and returns one plain `content` text block with no `structuredContent`.
    - CLI results are capped at 19,000 bytes, below the 20 KiB point where the CLI saves output to a file; continue a truncated item from its `next:` marker.
    - The server's `instructions` name `view`, `grep`, `bash`/`powershell`, `web_fetch`, `edit`, `create`, and `apply_patch` as the native routes to replace or keep.

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

1. **GitHub Copilot in VS Code**
    - Add MCP servers: <https://code.visualstudio.com/docs/agent-customization/mcp-servers>
    - MCP configuration formats: <https://code.visualstudio.com/docs/agents/reference/mcp-configuration>
    - Custom instructions: <https://code.visualstudio.com/docs/agents/reference/custom-instructions-reference>
    - Built-in tool references: <https://code.visualstudio.com/docs/agents/reference/tools-reference>
    - Tool approvals: <https://code.visualstudio.com/docs/agents/run/approvals>
2. **GitHub Copilot CLI**
    - Add MCP servers and `mcp-config.json`: <https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-mcp-servers>
    - Command options, tool names, permission patterns, MCP naming, and instruction files: <https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference>
    - Allowing and denying tools: <https://docs.github.com/en/copilot/how-tos/copilot-cli/use-copilot-cli/allowing-tools>
    - Configuration directory and `COPILOT_HOME`: <https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-config-dir-reference>
    - Large tool output: <https://docs.github.com/en/copilot/concepts/prompting-and-context/context-management>
