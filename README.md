# rust-fs-mcp

## Overview

1. **What it is**
    - A native Rust stdio [MCP](https://modelcontextprotocol.io) server that gives an AI agent one batch-first toolset for local files, search, git, and the web.
    - It speaks JSON-RPC over stdin/stdout and ships as a single self-contained binary.
    - Tool names and request shapes follow the public fs-mcp contract.
2. **Supported agents**
    - Only these five MCP hosts are supported: Claude Code, OpenAI Codex, GitHub Copilot (VS Code agent mode and Copilot CLI), Gemini CLI, and OpenCode.
    - Each one has its own routing guide under `docs/<agent>/FS.md`; other MCP clients may connect but are not supported.
3. **Highlights**
    - One binary with no Node, Python, or `rg` runtime; `git` on PATH is needed only for the git tools, and the optional `ast-grep` CLI only for `file-replace` `mode: "ast"`.
    - 24 tools across files, directories, paths, edits, content search, git, and the web.
    - An optional 25th tool, the read-only `mysql-query`, is listed only when config.toml has a `[mysql]` section; its MySQL wire client is built in, with no driver crate.
    - In-process path finding with `fd` semantics, pattern replacement with `sd` semantics, and unified diffs on the `similar` library.
    - Batch-first input: same-kind operations take an `items[]` or `paths[]` array and run on a pooled parallel executor.
    - In-process content search on ripgrep's own libraries (`grep-searcher`, `ignore`).
    - A tokio-free HTTPS client behind a per-hop SSRF guard.
    - One compact response envelope, size-bounded to stay under an MCP client's output-token cap.
4. **Documents**
    - Korean version: [README-KO.md](README-KO.md)
    - Source tree: [ARCHITECTURE.md](ARCHITECTURE.md)
    - Routing guides for agent instruction files: [Claude Code](docs/claude/FS.md), [Codex](docs/codex/FS.md), [GitHub Copilot](docs/copilot/FS.md), [Gemini CLI](docs/gemini/FS.md), [OpenCode](docs/opencode/FS.md)
    - License: [LICENSE.md](LICENSE.md)

---

## Quick Start

### 1. Download

1. **Release archive**
    - Download the archive for your platform from the [latest release](https://github.com/jungho-dev/rust-fs-mcp/releases/latest).
    - Direct URL pattern: `https://github.com/jungho-dev/rust-fs-mcp/releases/download/<tag>/rust-fs-mcp-<target>.zip`
2. **Windows targets**
    - `x86_64-pc-windows-msvc`
    - `aarch64-pc-windows-msvc`
3. **macOS targets**
    - `x86_64-apple-darwin` (Intel)
    - `aarch64-apple-darwin` (Apple Silicon)
4. **Linux targets**
    - `x86_64-unknown-linux-gnu`
    - `aarch64-unknown-linux-gnu`

### 2. Verify

1. **Checksum**
    - Every archive ships with a matching `<archive>.sha256sum` file.
    - Verify the archive before extracting it.

```bash
# macOS and Linux
shasum -a 256 -c rust-fs-mcp-x86_64-unknown-linux-gnu.zip.sha256sum
```

```powershell
# Windows PowerShell: prints True when the hash matches
$zip = ".\rust-fs-mcp-x86_64-pc-windows-msvc.zip"
(Get-FileHash -Algorithm SHA256 $zip).Hash -eq (Get-Content "$zip.sha256sum" -Raw).Split(" ")[0]
```

### 3. Register

1. **Server command**
    - The server takes no arguments; the client only needs the absolute path to the binary.
    - The binary also accepts an internal `--isolated-worker` flag that only the server passes when it spawns its own fancy-regex worker; never put it in a client config.
    - On Windows, point to `rust-fs-mcp.exe` and use forward slashes or escaped backslashes in JSON.
2. **JSON config**
    - Claude Code, Claude Desktop, and most other MCP clients accept this `mcpServers` shape.

```json
{
  "mcpServers": {
    "rust-fs-mcp": {
      "command": "/absolute/path/to/rust-fs-mcp"
    }
  }
}
```

### 4. Register With Claude Code

1. **CLI command**
    - `claude mcp add` registers the same server without editing JSON; `--scope user` makes it available in every project.

```bash
claude mcp add --scope user rust-fs-mcp -- /absolute/path/to/rust-fs-mcp
```

### 5. Check

1. **Restart**
    - Restart the client; the 24 tools appear in `tools/list`, or 25 with `mysql-query` when config.toml has a `[mysql]` section with `host` and `user`.
2. **Protocol version**
    - The server negotiates `2024-11-05`, `2025-03-26`, or `2025-06-18` automatically, so the client needs no version setting.
    - `ping` returns an empty result, and `logging/setLevel` accepts the standard MCP log levels.
3. **Agent routing**
    - Load the routing guide for your agent from its instruction file so the agent prefers these tools over its built-in ones:
        1) Claude Code: [docs/claude/FS.md](docs/claude/FS.md), imported from `CLAUDE.md`;
        2) Codex: [docs/codex/FS.md](docs/codex/FS.md), pasted into `AGENTS.md`;
        3) GitHub Copilot: [docs/copilot/FS.md](docs/copilot/FS.md), pasted into `.github/copilot-instructions.md` or a `*.instructions.md` file in VS Code, or imported with `@FS.md` in Copilot CLI;
        4) Gemini CLI: [docs/gemini/FS.md](docs/gemini/FS.md), imported from `GEMINI.md`;
        5) OpenCode: [docs/opencode/FS.md](docs/opencode/FS.md), listed in the `instructions` array of `opencode.json`.

---

## Tools

### 1. Catalog

1. **Files and directories**
    - `file-read`, `file-read-line-range`, `file-write`, `dir-create`, `dir-list`
2. **Paths and edits**
    - `path-copy`, `path-move`, `path-remove`, `path-stat`, `file-edit`, `file-edit-lines`, `file-replace`, `file-diff`
3. **Search**
    - `fs-search`, `path-find`
4. **Git**
    - `git-status`, `git-add`, `git-commit`, `git-amend`, `git-diff`, `git-show`
5. **Web**
    - `web-fetch`, `web-extract`, `download-to-file`
6. **Database**
    - `mysql-query`, listed only when config.toml has a `[mysql]` section with `host` and `user`
7. **Annotations**
    - Every tool carries a human-readable `title`, such as `Stage Git Files`, at the top level and in `annotations.title`.
    - `readOnlyHint: true`: `file-read`, `file-read-line-range`, `dir-list`, `fs-search`, `path-find`, `path-stat`, `file-diff`, `git-status`, `git-diff`, `git-show`, `web-fetch`, `web-extract`
    - `destructiveHint: true`: `file-write`, `file-edit`, `file-edit-lines`, `file-replace`, `path-move`, `path-remove`, `git-add`, `git-commit`, `git-amend`, `download-to-file`
    - `destructiveHint: false`: `dir-create`, `path-copy`
    - `git-add` counts as destructive because `all: true` stages deletions and a path add replaces partially staged hunks.
    - `openWorldHint: true`: `file-read` (for `isUrl` reads), `web-fetch`, `download-to-file`; every other tool declares `false`.
    - `mysql-query`, when listed, declares `readOnlyHint: true` and `openWorldHint: false`.

### 2. Files And Directories

1. **file-read**
    - Reads text, binary, image, and directory targets in parallel.
    - Each item takes an optional `offset`/`length` slice.
    - `isUrl: true` fetches an HTTP/HTTPS URL through the shared web client.
2. **file-read-line-range**
    - Returns a 1-based line range (`start_line`, optional `line_count`) with line numbers, using native streaming.
3. **file-write**
    - Rewrites or appends (`mode`); large content can come from `content_path`.
4. **dir-create**
    - Creates one or many directories.
5. **dir-list**
    - Lists directories with `depth`, `maxEntries`, `includeFiles`, `excludePatterns`, and `allowMissing`.
    - Hides `node_modules/`, `target/`, and `.git/` unless the listed path is inside one; `noDefaultExcludes: true` shows them.
6. **Legacy encodings**
    - Reads and edits decode UTF-16 (BOM) and EUC-KR/CP949 automatically and write edits back in the original encoding.
    - The detected encoding is reported as `encoding`.

### 3. Paths And Edits

1. **path-copy, path-move, path-remove**
    - Copy, move or rename, and delete with `recursive`/`force` flags.
    - Independent paths run in parallel; overlapping paths fall back to a sequential runner.
    - `path-copy` refuses to copy a directory into itself or its own subtree.
2. **path-stat**
    - Returns metadata for many paths at once.
3. **file-edit**
    - Applies exact block replacements (`old_string` → `new_string`).
    - `old_string` must match exactly `expected_replacements` times (default 1); otherwise the edit fails with the matching line numbers and changes nothing.
    - An empty `old_string` is rejected.
    - When a single-line `old_string` is replaced by multi-line text, the new lines take the file's dominant line ending.
4. **file-edit-lines**
    - Replaces, inserts, or deletes by 1-based line number.
    - Preserves the file's original line endings, including a missing final newline.
5. **dryRun**
    - `file-edit` and `file-edit-lines` items take `dryRun` (default `false`): every validation and the encode step run, and only the write is skipped.
    - The result is `Would edit ...` plus a unified diff; an identical edit says `No changes`, and a line-ending-only edit says `Only line endings would change` with `eolChanged`.
    - A batch with any `dryRun` item uses the full multi-line output; without `dryRun` the responses are unchanged.
6. **file-replace**
    - Runs regex or literal find-and-replace inside files with `sd` semantics, one item per `file_path`; `replacement` is required, and `""` deletes the match.
    - `mode: "regex"` (default) is always multi-line and case-sensitive unless `ignoreCase: true`, not smart-case like `fs-search`; `dotAll` and `wordMatch` are optional.
    - Look-around and backreferences switch to fancy-regex; an invalid regex errors with no literal fallback.
    - Each item takes `timeout_ms` (default 25000) and has a 256 MiB result cap.
    - A fancy-regex item runs in a kill-able worker process that streams match spans back as it finds them, so only the result cap and `maxReplacements` bound it; the worker is killed at the item's `timeout_ms`, and a kill or worker crash fails the item, writes nothing, and leaves the server running.
    - The replacement expands `$1`, `${1}`, `${name}`, and `$$`; an ambiguous `$1a` or `$1_x` and an unknown group reference are rejected.
    - `mode: "literal"` inserts the replacement verbatim; `mode: "ast"` rewrites by ast-grep pattern and template, keeping the outermost of nested matches like `ast-grep -U`.
    - `maxReplacements` caps the matches in file order (`0` replaces all, like `sd -n`); `expectedReplacements` writes nothing on a count mismatch.
    - Every item is planned in memory before any write, so one failing item writes nothing; same-file items apply in order.
    - `dryRun: true` on any item previews the whole call; applied edits return each item's unified diff too.
    - UTF-8 (BOM kept), UTF-16 LE/BE BOM, and EUC-KR/CP949 files are written back in their original encoding.
    - Differences from `sd`:
        1) `wordMatch` wraps the whole pattern as `\b(?:...)\b`, so an alternation stays word-bounded;
        2) `^` and `$` work on CRLF files;
        3) look-around and backreferences work instead of erroring;
        4) an unknown group reference errors instead of inserting empty text;
        5) backslash escapes such as `\n` in the replacement are kept verbatim, not unescaped.
7. **file-diff**
    - Compares `path` with `otherPath`, or with inline `content` or `content_path`, as a git-style unified diff without git.
    - Options: `contextLines` (3), `ignoreWhitespace` (`git diff -b`), and `algorithm` `myers` (default), `patience`, or `histogram`.
    - CRLF and LF are folded before comparing and reported as `eolChanged`; binary files are refused, and each side is limited to 64 MiB.
    - Both sides decode UTF-8, UTF-16, and EUC-KR.
    - A long diff is capped near the output budget with `truncated: true` while the insertion and deletion counts stay exact.
8. **ast-grep lookup**
    - The server resolves the `ast-grep` CLI for `file-replace` `mode: "ast"` in this order:
        1) `RUST_FS_MCP_AST_GREP`, absolute or relative to the server's working directory; a `.cmd`, `.bat`, or `.ps1` wrapper resolves to the npm native executable or errors;
        2) on Windows, the server executable's folder, System32, and the Windows folder, in the Rust standard `Command` order;
        3) absolute `PATH` entries, skipping empty and relative ones;
        4) the native executable of an npm `@ast-grep/cli` install next to an `ast-grep.cmd` shim.
    - On Windows the child runs in a Job Object, so a timeout also ends its descendants; on Unix it gets its own process group, killed as a whole.
    - When the server has a console, as under Claude Code's hidden conhost, the child inherits it; without one the server keeps `CREATE_NO_WINDOW`, about 7 ms more.

### 4. Search

1. **fs-search**
    - Runs ripgrep-compatible regex content search in-process and returns the batch result directly.
2. **Flags**
    - `ignoreCase`, `literal` (`rg -F`), `wordMatch` (`rg -w`), `multiline` (`rg -U`), `contextLines`, `includeHidden`, `filePattern`, `maxResults`, `maxColumns`, `respectGitignore`, `encoding`
3. **Defaults**
    - Smart-case when `ignoreCase` is omitted.
    - Repository `.gitignore` files are respected; `respectGitignore: false` includes ignored files.
    - Lines longer than `maxColumns` (500) are clipped around the match.
4. **Results**
    - Results are sorted by path.
    - Collection stops at the response budget with `limitReached: true`.
    - A timeout returns the matches found so far with `timedOut: true`.
    - A missing search path is an error, not zero matches.
5. **Pattern flavor**
    - Rust regex: a linear-time engine with Unicode-aware `\d \w \b`, so Hangul word boundaries work.
    - Look-around and backreferences switch to fancy-regex, bounded by a backtrack limit and run in a kill-able worker process stopped at `timeout_ms` + 1 s; a kill or worker crash fails only that item, and the server keeps running.
    - An invalid regex, including a fancy-regex syntax error, falls back to a literal search once, and the backend label names the error at its position in your pattern.
    - Engine limits, such as an unsupported Unicode-mode change, the recursion depth, or the compiled size, stay errors.
    - `wordMatch` validates the pattern alone before wrapping it, so an unbalanced `)` is a syntax error and `(?x)` comments work.
6. **Other behavior**
    - Binary files are skipped.
    - Large patterns can come from `pattern_path`.
    - `^` and `$` match at line edges in CRLF files as in LF files, so `off$`, `^$`, and `^\s*$` return the same lines for both.
    - A line-by-line search drops the one terminating CR of each line before matching, so `\r` or `\r$` does not match at a CRLF line end; set `multiline: true` to match the CR itself, as in `off\r\n`.
7. **path-find**
    - Finds files and directories by name regex or glob with `fd` semantics, without reading contents, on the same `ignore` walk as `fs-search`.
    - `pattern` is a smart-case regex, and `glob` is a glob with `|` alternatives; both match the entry name, directories included.
    - `fullPath: true` matches the root-relative `/` path instead, where `*` stops at `/`, so use `**/*.rs` for any depth.
    - Hidden entries are skipped unless `includeHidden: true` or the pattern or glob starts with `.`.
    - Repository `.gitignore` files are respected like `fs-search`; `node_modules/`, `target/`, and `.git/` are skipped unless `noDefaultExcludes: true` or the root is inside one.
8. **path-find filters**
    - `type` (`any`, `file`, `directory`, `symlink`), `extension` (`rs|toml`), `empty`, `minDepth`/`maxDepth`, `excludePatterns` (`fd --exclude`), and `followLinks`.
    - `size` follows `fd -S`: `+10k` at least, `-1M` at most, `500b` exactly; a unit is required, `k` is 1000 and `ki` is 1024, and only files match.
    - `changedWithin` and `changedBefore` take a duration (`30m`, `2h`, `7d`, `2w`) or a date (`YYYY-MM-DD`, `YYYY-MM-DD HH:MM:SS`, RFC 3339, `@epoch`).
    - A date without an offset is local time, like `fd`: a DST overlap takes the later instant, and a gap takes the pre-transition offset.
    - `TZ` is honored as a TZif path, an IANA name, or a POSIX rule; without `TZ` the system zone applies, and on Windows an IANA name needs ICU (Windows 10 1703 or later) or the date errors.
9. **path-find output**
    - The first line is `<root>:`, then root-relative entries with `/` separators; directories end with `/`.
    - `details: true` appends a tab, the size in bytes, a tab, and the UTC RFC 3339 modification time.
    - `sortBy` is `path` (default, component-wise), `modified` (newest first), or `size` (largest first); `maxResults` keeps the exact first entries in that order.
    - `limitReached: true` or `timedOut: true` marks partial results.

### 5. Git

1. **Execution**
    - Every git tool requires a `path` and runs the git CLI inside that worktree.
    - `git` is resolved from absolute PATH entries only.
    - Git children run with `GIT_TERMINAL_PROMPT=0`, so git never prompts for credentials on the console.
    - On Windows a git child inherits the server console when one is attached and gets `CREATE_NO_WINDOW` otherwise.
    - A missing path reports `Path does not exist`, a path git does not recognize as a repository reports `Not a git repository`, and any other failure reports `Cannot open the git repository at <path>` with git's own message.
2. **git-status**
    - Runs `status --porcelain --branch`.
3. **git-add**
    - Stages paths, with `all`/`update`/`force`.
4. **git-commit**
    - Injects a default committer identity so commits succeed without local git config.
    - Accepts an optional author override and supports `amend`/`allow-empty`/`no-verify`.
    - The message must start with a Conventional Commit header: a lowercase English type, with the summary in any language.
5. **git-amend**
    - Rewrites the last commit: `--no-edit` when no message is given, otherwise a validated new message.
    - Supports author override or reset-author, staging, `allow-empty`, and `no-verify`.
6. **git-diff**
    - Runs `git diff` with `staged`, `nameOnly`, `stat`, `source`/`target`, `contextLines`, `check`, and path filters.
7. **git-show**
    - Renders an object (or `objects[]`) and an optional `object:path` through `git show`.
8. **Option guard**
    - Revision inputs that begin with `-` are rejected, so a revision cannot smuggle a git option.

### 6. Web

1. **web-fetch**
    - Fetches one or many URLs over HTTP/HTTPS without a browser.
    - Output modes: `markdown` (default), `text`, `links`, `readability` (main content), or raw `html`.
    - Pages long rendered text with `offset`/`maxChars`.
    - Relative links and redirect targets resolve per RFC 3986: query-only and fragment-only references, dot segments, and `//host` forms; a reference that carries its own scheme, in any letter case and including non-HTTP schemes such as `mailto:`, is kept as written.
2. **web-extract**
    - Converts HTML you already hold (inline or a local file) into markdown, text, links, or readability, fully offline.
    - HTML nested deeper than 256 levels is flattened at that depth; the same parser serves `web-fetch`.
3. **download-to-file**
    - Downloads one or many URLs, streamed to a temp file and renamed into place.
    - `overwrite: true` replaces an existing file through that rename without deleting it first, so a failed download or rename leaves the original in place.
    - Items that share a destination in one batch run in order instead of in parallel; without `overwrite` the later item fails with `File already exists`.
4. **SSRF guard**
    - `web-fetch`, `download-to-file`, and `file-read` with `isUrl` resolve the host and reject non-public addresses: private, link-local, unique-local, CGNAT, multicast/reserved, and embedded-IPv4 IPv6 forms.
    - The check repeats on every redirect hop.
    - Loopback (`localhost`, `127.0.0.0/8`, `::1`) is allowed for local development.
    - Validated addresses are pinned into the connection resolver, so a DNS answer that changes between check and connect cannot redirect the request.
5. **Retry**
    - `web-fetch`, `download-to-file`, and `file-read` with `isUrl` retry a connection reset that arrives before any response byte once, because these GET requests are idempotent; a second reset is reported as an error.
6. **Body size**
    - `web-fetch` defaults to 20,000,000 bytes.
    - `download-to-file` defaults to the 200,000,000-byte hard ceiling.
    - An explicit `maxBytes` can move either value up to that ceiling.

### 7. Database

1. **mysql-query**
    - Runs one read-only SQL statement per call against the MySQL server set in the `[mysql]` section of config.toml.
    - `tools/list` includes it only when that section has `host` and `user`; a call without the section returns an error that tells how to enable it.
    - Allowed statements: `SELECT`, `SHOW`, `DESCRIBE`/`DESC`, `EXPLAIN`, and `WITH ... SELECT`.
    - Read the schema first (`SHOW`, `DESCRIBE`, `information_schema` filtered by `TABLE_SCHEMA` and `TABLE_NAME`) and keep every `SELECT` small with `LIMIT`.
2. **Read-only gate**
    - The gate rejects a statement before it reaches the server:
        1) more than one statement in a call;
        2) executable comments (`/*! ... */`, `/*M! ... */`);
        3) optimizer hints that change session variables or the time limit (`SET_VAR`, `MAX_EXECUTION_TIME`);
        4) `INSERT`, `UPDATE`, `DELETE`, or `REPLACE` behind `WITH`;
        5) locking reads (`FOR UPDATE`, `FOR SHARE`, `LOCK IN SHARE MODE`);
        6) `SLEEP`, `BENCHMARK`, lock functions such as `GET_LOCK`, replication wait functions, and `LOAD_FILE`;
        7) `EXPLAIN ANALYZE`;
        8) `INTO OUTFILE` and `INTO DUMPFILE`.
    - Keywords inside quoted text and backtick identifiers do not count, so `SELECT 'FOR UPDATE'` passes.
3. **Execution**
    - Every statement runs inside `START TRANSACTION READ ONLY` ... `ROLLBACK`, on a connection that never negotiates multi-statement support.
    - The session carries a server-side time limit: `MAX_EXECUTION_TIME` on MySQL, with `max_statement_time` as the MariaDB fallback.
    - A client-side timeout 2 seconds past that limit drops the connection and sends `KILL QUERY` from a second connection.
    - Calls are serialized on one connection per server process; an idle connection is reused for 60 seconds and then closed.
    - A connection that timed out, broke, or returned a truncated result is closed instead of reused.
4. **Arguments**
    - `sql`: the statement; `sql_path` reads it from a UTF-8 file instead.
    - `maxRows`: row cap for the call, never above `mysql.max_rows`.
    - `timeout_ms`: time limit for the call, never above `mysql.query_timeout_ms`.
5. **Results**
    - Rows return as a JSON array with one object per line, keys in `SELECT` order; a repeated column name gets a `_2`, `_3` suffix.
    - Integers and floats are JSON numbers; `DECIMAL` and temporal values stay strings so no digit is lost.
    - JSON columns are embedded as JSON, `NULL` is `null`, and binary values that are not printable UTF-8 become `0x` hex, shown up to 256 bytes.
    - A result cut at `maxRows` or at half the output budget reports `truncated: true`; the metadata also carries `rowCount`, `columnCount`, and `queryMs`.
6. **Wire client**
    - The client is built in, with no driver crate: plain TCP without TLS, and the text protocol (`COM_QUERY`).
    - Authentication supports `mysql_native_password` and `caching_sha2_password`, both the fast path and RSA full authentication.
7. **Security limits**
    - The connection has no TLS, so statements and rows cross the network unencrypted; use it only on a trusted network.
    - The gate is defense in depth, not the boundary: give the tool a database account that is itself read-only.

---

## Usage

### 1. Batch Input

1. **One call per kind**
    - Pass every same-kind target in one call.
    - Keep read batches near 3-8 files and split larger sets.
    - An oversized item is truncated on its own; the call is never rejected.

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"file-read","arguments":{"paths":["src/main.rs","src/lib.rs","Cargo.toml"]}}}
```

### 2. Forgiving Arguments

1. **Shape fixes**
    - The dispatcher absorbs common shape slips before a tool runs:
        1) a single flat operation is wrapped into `items[]`;
        2) key aliases are normalized (`file_path` → `path`, `from`/`to` → `source`/`destination`);
        3) `path-remove` and the metadata tools accept a plain `paths[]` array as well as `items[]`;
        4) an array sent as a JSON-encoded string is parsed back into an array.

### 3. Paths

1. **Resolution**
    - Prefer absolute paths.
    - Relative paths resolve against the server's working directory.
    - A leading `~` expands to the home directory.
    - A path wrapped in matching quotes, such as `"C:/work/a.txt"`, is unwrapped when the inner value is absolute, because some models send JSON strings with the quotes still inside.
2. **Access scope**
    - There is no allowed-root restriction; the server reaches any path the process user can.
    - Git tools require an explicit `path` on every call.

### 4. Large Arguments

1. **Argument file**
    - Any tool can read its arguments from a JSON file: `{"args_path":"/abs/path/to/args.json"}`.
    - Optional `args_offset`/`args_length` slice that file.
2. **Per-field files**
    - Large values can come from `content_path`, `old_string_path`, `new_string_path`, `replacement_path`, or `pattern_path`.

---

## Responses

### 1. Envelope

1. **Shape**
    - In the default `off` mode every call returns one compact envelope: `{data, durationMs}`, plus `error` only on failure.
    - Lossless mode and the lossless host profiles return plain `content` text instead, as Host Profiles and Configuration describe.
2. **Fields**
    - `data.content`: the result body (file contents, search lines, diffs, listings), exactly once.
    - `data.structuredContent`: tool-specific metadata only (counts, paths, backends); it never duplicates the body.
    - `durationMs`: the tool's wall-clock duration.
    - `_meta.fsMcpResult`: status, duration, content type, and whether structured content is present.
    - `isError`: set on tool failures.
3. **Batch results**
    - Batch tools add per-item `{index, ok, data}` entries plus `succeededCount`, `failedCount`, and `totalCount`.
    - A batch is marked as an error only when every item fails.

### 2. Host Profiles

1. **Detection**
    - At `initialize` the server matches `clientInfo.name` exactly and applies that host's profile to later calls; a new `initialize` replaces it.
    - Each `initialize` writes one stderr line: `rust-fs-mcp: initialize client="<name>" version="<ver>" protocol="<requested>" host=<host>`.
2. **Profiles**
    - Format: client name, host, result shape, output budget:
        1) `codex-mcp-client`: Codex, lossless, 30,000 bytes;
        2) `Code - OSS`, `Visual Studio Code`, `Visual Studio Code - Insiders`: GitHub Copilot in VS Code, lossless, 30,000 bytes;
        3) `copilot-cli`: GitHub Copilot CLI, lossless, 19,000 bytes;
        4) `gemini-cli-mcp-client`: Gemini CLI, lossless, 30,000 bytes;
        5) `opencode`: OpenCode, lossless, 30,000 bytes;
        6) `claude-code` or any other name: default envelope, 30,000 bytes.
    - The second line of the server `instructions` names that host's built-in tools to replace or keep, such as `exec_command` and `apply_patch` for Codex.
    - With `[mysql]` configured, the `instructions` end with one more line that routes database reads to `mysql-query`.
3. **Reason**
    - Claude Code passes the serialized `structuredContent` to the model, so the default envelope keeps the body there.
    - Gemini CLI reads only `content`, OpenCode uses `content` whenever it is non-empty, VS Code shows serialized `structuredContent` in place of the text, and Codex prefers `structuredContent` and drops images, so those hosts get the body in `content`.
    - Copilot CLI saves tool output over 20 KiB to a file and hands the model only a path and a preview, so its budget stays below that.
4. **Verified names**
    - A 2026-11-07 run against live hosts recorded `claude-code` (Claude Code 2.1.318), `codex-mcp-client` (Codex 0.166.0), `copilot-cli` (Copilot CLI 1.0.107), `gemini-cli-mcp-client` (Gemini CLI 0.67.0), and `opencode` (OpenCode 2.2.5).
    - VS Code was not launched: `Code - OSS` comes from the VS Code source, and the two official names are the documented product names.
    - `gemini mcp list` checks servers with a test client named `mcp-test-client`, which gets the default envelope; a Gemini CLI session sends `gemini-cli-mcp-client`.
    - An unmatched name falls back to the default envelope; the stderr line shows the name a host actually sends.

### 3. Output Budget

1. **Default**
    - A result is bounded server-side to 30,000 bytes, or 19,000 bytes for Copilot CLI, so it stays under the host's MCP output cap, such as Claude Code's 25,000-token default.
2. **Override order**
    - The first value found wins:
        1) `output.max_bytes` in config.toml;
        2) the legacy `RUST_FS_MCP_OUTPUT_BYTES` environment variable;
        3) `MAX_MCP_OUTPUT_TOKENS`, scaled at 1.2 bytes per token;
        4) the host profile default, 19,000 bytes for Copilot CLI.
    - With none of them set, the budget is 30,000 bytes.
    - The `anthropic/maxResultSizeChars` `_meta` in `tools/list` always uses the Claude Code budget, whatever host is connected.
3. **Truncation**
    - An oversized body is cut in place, measured in JSON-escaped bytes, with a notice and `outputTruncated`.
    - Batch items share the budget water-filling style.
    - A truncated `file-read` or `web-fetch` item ends with `next: offset=N`; a truncated `file-read-line-range` item ends with `next: start_line=N`.
    - A failed call also carries its error reason in the plain `content` text.

### 4. Configuration

1. **File location**
    - `~/.config/rust-fs-mcp/config.toml`, read once per process; restart the server after editing it.
    - `RUST_FS_MCP_CONFIG` overrides the path; an empty value or `none` skips the file.
    - A missing file keeps the defaults; an invalid key keeps its default and logs a warning to stderr.
2. **Lossless mode**
    - `compress = "lossless"` returns the result as one plain `content` text block with no `structuredContent`, because Claude Code forwards `structuredContent` instead of `content` when both exist.
    - Bodies skip JSON escaping and repeated metadata; structured fields the text lacks follow as one `[meta] {...}` line.
    - Duration and status move to `_meta.fsMcpResult`.
    - Result bodies are never rewritten, so exact-content guarantees hold.
    - `compress = "off"` forces the default envelope for every host; leaving `compress` unset follows the host profile.
3. **Tool usage stats**
    - `[stats] enabled = true` records tool call counts as a per-day table in `<dir>/YYYY-MM-DD.md`; unset or `false` costs one settings check per call.
    - `dir` defaults to `.docs`; a relative path resolves against the server working directory.
    - The call path only bumps an atomic counter; a background thread merges counts into the file every 2 seconds and once more when stdin closes.
    - Server processes share one file through the `.tool-stats.lock` lock file; a killed process loses the counts since its last merge.
4. **MySQL connection**
    - A `[mysql]` section with `host` and `user` turns on `mysql-query`; without both keys the tool stays off and a warning goes to stderr.
    - `port` defaults to 3306, and `database` is the optional default schema.
    - The password comes from the first source set, in this order:
        1) `password_env`, the name of an environment variable that holds the password;
        2) `password_dpapi`, a file sealed with Windows DPAPI by the current user;
        3) `password`, plain text in the config file, discouraged.
    - `connect_timeout_ms` (default 10000) bounds the TCP connect.
    - `query_timeout_ms` (default 10000) and `max_rows` (default 1000) are the defaults and the upper bounds for the `timeout_ms` and `maxRows` arguments.
    - `password_dpapi` works on Windows only; other platforms report an error that points to `password_env`.

```toml
[output]
compress = "lossless" # "off" | "lossless"; unset follows the host profile
max_bytes = 30000 # replaces the RUST_FS_MCP_OUTPUT_BYTES env var

[stats]
enabled = true # default false
dir = "C:/project/.docs" # default ".docs"; relative paths resolve against the server working directory

[mysql]
host = "db.example.invalid" # required
port = 3306 # default 3306
user = "reader" # required
database = "sample_db" # optional default schema
password_env = "SAMPLE_DB_PASSWORD" # first choice: name of an environment variable
# password_dpapi = "~/.config/rust-fs-mcp/mysql-password.dpapi" # second choice: Windows DPAPI file
# password = "..." # last choice: plain text, discouraged
connect_timeout_ms = 10000 # default 10000
query_timeout_ms = 10000 # default 10000; upper bound for timeout_ms
max_rows = 1000 # default 1000; upper bound for maxRows
```

5. **DPAPI password file**
    - Seal the password once as the Windows user that runs the server; DPAPI ties the file to that user, so another account cannot unseal it.
    - The script reads the password without echoing it and writes only the sealed bytes; the target directory must already exist.
    - Point `password_dpapi` at the file the script writes.

```powershell
Add-Type -AssemblyName System.Security
$secure = Read-Host -AsSecureString "MySQL password"
$bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
$plain = [Text.Encoding]::UTF8.GetBytes([Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr))
[Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
$sealed = [Security.Cryptography.ProtectedData]::Protect($plain, $null, 'CurrentUser')
[IO.File]::WriteAllBytes("$HOME\.config\rust-fs-mcp\mysql-password.dpapi", $sealed)
```

---

## Build From Source

### 1. Requirements

1. **Toolchain**
    - Rust 1.88 or newer (edition 2024).
2. **Runtime**
    - `git` on PATH, only for the git tools.
    - The optional `ast-grep` CLI on PATH, only for `file-replace` `mode: "ast"`.

### 2. Host Build

1. **Output**
    - The binary lands at `target/release/rust-fs-mcp` (`rust-fs-mcp.exe` on Windows).

```bash
cargo build --release
```

### 3. Target Build

1. **Cross target**
    - Add the target and pass `--target` on a host that can link it; the release workflow builds each target on its native runner.

```bash
rustup target add aarch64-apple-darwin
cargo build --release --target aarch64-apple-darwin
```

---

## Limitations

1. **Git**
    - The git tools need `git` on PATH; there is no in-process git object store.
    - Behavior follows the installed git version, including its submodule and rename-detection defaults.
2. **Web**
    - There is no browser tier, so JavaScript-rendered (client-side) pages cannot be fetched.
3. **ast-grep**
    - `file-replace` `mode: "ast"` needs the `ast-grep` CLI (validated with 0.51.1), found as ast-grep lookup describes; without it the mode returns an install-guidance error, and every other tool is unaffected.
    - The server never falls back to `sg`, which is an unrelated command on Linux.
    - `ast-grep` parses UTF-8 only, so `mode: "ast"` accepts UTF-8 without BOM only.
    - Rust macro arguments such as `assert!` and `format!` are token trees, so patterns do not match inside them.
4. **fancy-regex**
    - Each fancy-regex item in `fs-search` or `file-replace` runs in a worker process, which costs about 3 ms per item with the warm standby worker (measured 2026-11-11: 8.6 ms against 5.6 ms without look-around); other calls never spawn one.
    - After the first fancy-regex job the server keeps one idle standby worker: the next job takes it and a replacement starts, so at most one idles.
    - On Windows the worker starts with `DETACHED_PROCESS`, so no console host starts per worker.
5. **Time zones**
    - On Windows an IANA name in `TZ` needs ICU (Windows 10 1703 or later); without it a `path-find` date without an offset errors instead of falling back to UTC.
6. **MCP surface**
    - MCP resources and resource templates return empty lists; the project focuses on tools.
7. **MySQL**
    - `mysql-query` connects over plain TCP without TLS, so use it only on a trusted network.
    - The read-only gate is defense in depth; the database account should itself be read-only.
    - Authentication covers `mysql_native_password` and `caching_sha2_password` only, and `password_dpapi` works on Windows only.

---

## Development

### 1. Checks

1. **CI checks**
    - These are the checks CI runs on every push.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

2. **Targets**
    - CI runs clippy, build, and tests on `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`, and `aarch64-apple-darwin`, each on its native runner.
    - A separate MSRV job runs `cargo check --locked --all-targets` on Rust 1.88.0.
    - The MSRV check and `cargo clippy --all-targets -- -D warnings` for x86_64 and aarch64 on Linux, macOS, and Windows MSVC were verified; the non-host targets were checked without linking.

### 2. Tests

1. **Test coverage**
    - Unit tests cover core behavior inside each module.
    - `tests/tool_matrix.rs` calls all 24 always-listed catalog tools through dispatch, so the catalog and the dispatcher stay aligned, and checks that `mysql-query` without a `[mysql]` section returns its enable-guidance error.
    - The MySQL client unit tests run against a test-only loopback server, so they need no database.
    - With the `ast-grep` CLI on PATH it runs the `file-replace` `mode: "ast"` rewrite test; without it the rewrite test is skipped.
    - `tests/isolation.rs` drives the real stdio server: a runaway fancy-regex search or replace is killed on time while the same server keeps answering, a crashed worker fails only its item, worker results match the in-process engine, only fancy items spawn a worker, and a worker exits when its parent goes away or its budget passes.
    - It also checks that the idle standby worker serves the next job, is killed at its job's timeout, and exits with its server.
2. **Test isolation**
    - `.cargo/config.toml` sets `RUST_FS_MCP_CONFIG=none` and clears the output-budget variables, so `cargo test` ignores your personal config.
3. **Benchmarks**
    - `bench/README.md` documents the stdio benchmark and the A/B comparison scripts.
    - `bench/cli-bench.ts` compares each task before and after the CLI integration: base tools or a spawned `fd`, `sd`, or `git` process against `path-find`, `file-replace`, `file-diff`, and `dryRun`.

---

## License

1. **Apache-2.0**
    - See [LICENSE.md](LICENSE.md).
