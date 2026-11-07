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
    - Shell listing and search (`ls`, `find`, `rg`, `grep`): `dir-list`, `path-stat`, or `fs-search`.
    - Shell git (`git status`, `git diff`, `git show`, `git add`, `git commit`): the matching `git-*` tool.
    - Shell downloads (`curl`, `wget`) for a URL body or file: `web-fetch` or `download-to-file`.
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
    - Line-range read: `file-read-line-range` with `start_line` and `line_count`.
    - Metadata or existence probe: `path-stat`, or `file-read` with `allowMissing: true`.
2. **Edit and write**
    - Targeted manual edit or deliberate full rewrite: `apply_patch`.
    - Generated or batched write: `file-write`.
    - Batched exact replacement: `file-edit`.
    - Known 1-based line edit: `file-edit-lines`.
3. **Paths and directories**
    - Copy, move, or remove: `path-copy`, `path-move`, `path-remove`.
    - Directory list or create: `dir-list`, `dir-create`.
4. **Search**
    - Text or regex search, single or batched, including code identifiers: `fs-search`.
    - When a code graph is available, structural questions (symbols, callers, dependencies) go to the graph first, and `fs-search` serves text checks.
5. **Git**
    - `git-status`, `git-diff`, `git-show`, `git-add`, `git-commit`, `git-amend`.
6. **Web**
    - URL fetch, single or batch: `web-fetch`.
    - Held HTML to text, markdown, links, or readability: `web-extract`.
    - URL download to a file: `download-to-file`.
7. **Large payloads**
    - Large read-only arguments: `*_path` or `args_path` indirection.
    - Large write or edit payloads: an inline target plus `content_path`, `old_string_path`, `new_string_path`, or `replacement_path`.

---

## Rules

1. **Paths and batching**
    - Use absolute paths; relative paths resolve against the server working directory.
    - Batch two or more same-kind operations into one `items[]` or `paths[]` call.
    - Keep read batches near 3-8 files per call and split larger sets.
    - Prefer `rust-fs-mcp` even for a single operation for git status and diff, directory listing or probing, and multi-file existence probes.
2. **Read size**
    - Read a large file by the needed range only: locate the span with `fs-search`, then read it with `file-read-line-range`.
    - Oversized results are truncated per item, never rejected.
    - A truncated item ends with `next: offset=N` or `next: start_line=N`; continue from there instead of rereading.
    - Use `allowMissing: true` when probing possibly missing paths.
3. **Write targets**
    - Keep every write or edit target inline.
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
    - `writes` prompts only for tools not marked read-only; the server marks `file-read`, `file-read-line-range`, `dir-list`, `fs-search`, `path-stat`, `git-status`, `git-diff`, `git-show`, `web-fetch`, and `web-extract` with `readOnlyHint: true`.
    - `tools.<tool>.approval_mode` overrides one tool.
3. **Tool selection and output**
    - `enabled_tools` is an allow list and `disabled_tools` a deny list applied after it.
    - `tools.<tool>.output_token_limit` sets a token budget for one tool's output.
4. **Result shape**
    - The server matches the `initialize` client name `codex-mcp-client` (recorded from Codex 0.166.0) and returns each result as one plain `content` text block with no `structuredContent`, so the body and images reach the model.
    - Codex results keep the default 30,000-byte budget; in a Codex 0.166.0 check, 47 KB and 58 KB MCP results reached the model intact, so the server budget, not Codex, cuts large results.
    - Continue a truncated item from its `next:` marker instead of widening the call.
    - The server's `instructions` name `exec_command`, `apply_patch`, and `web_search` as the native routes to replace or keep.
    - An explicit `compress` or `max_bytes` in the server's config.toml overrides these defaults.

---

## Known Cost

1. **Git**
    - Multi-revision `git-show` is efficient with `objects[]` and `stat: true`.
    - Use `git-diff` with `nameOnly` for changed-file lists.
2. **Web**
    - `rust-fs-mcp` has no JavaScript-rendering tool, so client-rendered (SPA) pages cannot be fetched through it.

---

## Host Docs

1. **Codex**
    - MCP setup and `config.toml` server options: <https://developers.openai.com/codex/mcp>
    - `AGENTS.md` discovery: <https://developers.openai.com/codex/guides/agents-md>
    - Configuration reference: <https://developers.openai.com/codex/config-reference>
