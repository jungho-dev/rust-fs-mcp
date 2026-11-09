# Architecture

## Source Tree

```markdown
rust-fs-mcp/
├── src/
│   ├── main.rs : binary entry, starts the stdio server or the --isolated-worker job runner
│   ├── lib.rs : public facade for the core, protocol, and tools modules
│   ├── protocol/
│   │   ├── mod.rs : protocol module gateway
│   │   ├── server.rs : JSON-RPC line loop, version negotiation, per-host instructions, ping, concurrent tools/call dispatch
│   │   └── catalog.rs : tool names, titles, descriptions, input schemas, annotations
│   ├── tools/
│   │   ├── mod.rs : tool-name dispatcher, legacy module aliases
│   │   ├── arg_shape.rs : argument-shape absorption before dispatch
│   │   ├── common.rs : argument readers and path rules shared by all domains
│   │   ├── fs/
│   │   │   ├── mod.rs : fs domain gateway, cross-handler tests
│   │   │   ├── read.rs : file-read, encodings, images, isUrl reads
│   │   │   ├── lines.rs : file-read-line-range
│   │   │   ├── write.rs : file-write, atomic temp-file writer
│   │   │   ├── dir.rs : dir-create, dir-list, exclude patterns
│   │   │   ├── path_ops.rs : path-copy, path-move, path-remove, path-stat
│   │   │   ├── edit.rs : file-edit, file-edit-lines, dryRun previews
│   │   │   ├── replace.rs : file-replace, sd-style regex, literal, and ast rewrites, all-or-nothing writes
│   │   │   ├── compare.rs : file-diff, git-free unified diff
│   │   │   └── shared.rs : item readers, touched-path extractors
│   │   ├── search/
│   │   │   ├── mod.rs : fs-search handler, engine choice
│   │   │   ├── matcher.rs : regex and fancy-regex matchers, smart-case
│   │   │   ├── walk.rs : parallel walk, grep sink, budgets
│   │   │   ├── find.rs : path-find, fd-style name, type, size, and time filters
│   │   │   ├── ast.rs : ast-search over the ast-grep CLI, rewrite edits for file-replace
│   │   │   └── tests.rs : fs-search end-to-end tests
│   │   ├── git/
│   │   │   ├── mod.rs : git domain gateway
│   │   │   ├── repo.rs : git CLI runner, toplevel cache
│   │   │   ├── worktree.rs : git-status, git-add
│   │   │   ├── commit.rs : git-commit, git-amend, message checks
│   │   │   └── inspect.rs : git-diff, git-show
│   │   └── web/
│   │       ├── mod.rs : web domain gateway, shared field readers
│   │       ├── fetch.rs : web-fetch
│   │       ├── extract.rs : web-extract
│   │       └── download.rs : download-to-file
│   └── core/
│       ├── mod.rs : core module gateway
│       ├── args_ref.rs : args_path argument indirection
│       ├── batch.rs : batch runners, worker pool, batch result format
│       ├── client.rs : MCP host detection, per-host result policy
│       ├── config.rs : home expansion, path normalization
│       ├── diff.rs : shared unified-diff engine (similar), CRLF folding
│       ├── encoding.rs : UTF-16 and EUC-KR/CP949 decode and re-encode
│       ├── external.rs : git and ast-grep CLI spawn with timeouts, ast-grep binary resolution
│       ├── isolate.rs : kill-able worker process for fancy-regex jobs
│       ├── response.rs : RawResult, response envelope, per-host output budget
│       ├── settings.rs : config.toml loader
│       └── web.rs : HTTP client, SSRF guard, HTML extraction
├── tests/
│   ├── tool_matrix.rs : calls every catalog tool through dispatch
│   ├── isolation.rs : runaway fancy-regex kill and worker crash checks against the real binary
│   └── performance.rs : ignored local timing workload
├── bench/
│   ├── README.md : benchmark usage
│   ├── mcp-bench.ts : end-to-end stdio benchmark with correctness checks
│   ├── ab.ts : interleaved A/B comparison of two binaries
│   ├── cli-bench.ts : before/after benchmark of the CLI integration (fd, sd, similar, ast-grep)
│   ├── probe.ts : raw result-shape dump
│   └── probe-search.ts : fs-search metadata probe
├── docs/
│   ├── claude/FS.md : Claude Code routing guide
│   ├── codex/FS.md : Codex routing guide
│   ├── copilot/FS.md : GitHub Copilot routing guide
│   ├── gemini/FS.md : Gemini CLI routing guide
│   └── opencode/FS.md : OpenCode routing guide
├── .github/workflows/
│   ├── ci.yml : fmt, clippy, build, test per target, MSRV check
│   └── release.yml : per-target release archives and checksums
├── .cargo/config.toml : local linker, test environment isolation
├── .cmd/
│   ├── build.cmd : Windows release deploy
│   └── publish.cmd : crates.io publish
├── build.rs : copies libunwind.dll for gnullvm targets
├── Cargo.toml : package metadata, dependencies
├── README.md : usage guide
└── LICENSE.md : Apache-2.0
```
