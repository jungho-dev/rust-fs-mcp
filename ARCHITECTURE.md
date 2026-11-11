# Architecture

## Source Tree

```markdown
rust-fs-mcp/
├── src/
│   ├── main.rs : binary entry, starts the stdio server or the --isolated-worker job runner
│   ├── lib.rs : public facade for the core, protocol, and tools modules
│   ├── protocol/
│   │   ├── mod.rs : protocol module gateway, protocol::server re-export
│   │   ├── jsonrpc/
│   │   │   ├── mod.rs : JSON-RPC transport gateway
│   │   │   └── server.rs : JSON-RPC line loop, version negotiation, per-host instructions, ping, concurrent tools/call dispatch
│   │   └── catalog/
│   │       ├── mod.rs : tool catalog gateway
│   │       └── tools.rs : tool names, titles, descriptions, input schemas, annotations, mysql-query listed only when configured
│   ├── tools/
│   │   ├── mod.rs : tool-name dispatcher, legacy module aliases
│   │   ├── args/
│   │   │   ├── mod.rs : argument domain gateway
│   │   │   ├── arg_shape.rs : argument-shape absorption before dispatch
│   │   │   └── common.rs : argument readers and path rules shared by all domains
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
│   │   │   ├── ast.rs : ast-grep CLI rewrite edits for file-replace mode "ast"
│   │   │   └── tests.rs : fs-search end-to-end tests
│   │   ├── git/
│   │   │   ├── mod.rs : git domain gateway
│   │   │   ├── repo.rs : git CLI runner, toplevel cache
│   │   │   ├── worktree.rs : git-status, git-add
│   │   │   ├── commit.rs : git-commit, git-amend, message checks
│   │   │   └── inspect.rs : git-diff, git-show
│   │   ├── web/
│   │   │   ├── mod.rs : web domain gateway, shared field readers
│   │   │   ├── fetch.rs : web-fetch
│   │   │   ├── extract.rs : web-extract
│   │   │   └── download.rs : download-to-file
│   │   ├── sql/
│   │   │   ├── mod.rs : sql domain gateway
│   │   │   ├── query.rs : mysql-query handler, row and time limits capped by the config
│   │   │   ├── gate.rs : read-only statement gate
│   │   │   ├── session.rs : one cached connection, READ ONLY transaction, time limit, KILL QUERY on timeout
│   │   │   └── render.rs : result rows as JSON objects
│   │   └── stats/
│   │       ├── mod.rs : stats domain gateway
│   │       └── counter.rs : opt-in per-day tool call counter, day-file table writer
│   └── core/
│       ├── mod.rs : core module gateway, flat core::<name> re-exports
│       ├── call/
│       │   ├── mod.rs : tool-call plumbing gateway
│       │   ├── args_ref.rs : args_path argument indirection
│       │   ├── batch.rs : batch runners, worker pool, batch result format
│       │   └── response.rs : RawResult, response envelope, per-host output budget
│       ├── config/
│       │   ├── mod.rs : config gateway
│       │   ├── paths.rs : home expansion, path normalization
│       │   ├── settings.rs : config.toml loader ([output], [stats], [mysql])
│       │   └── client.rs : MCP host detection, per-host result policy
│       ├── process/
│       │   ├── mod.rs : child-process gateway
│       │   ├── external.rs : git and ast-grep CLI spawn with timeouts, ast-grep binary resolution
│       │   └── isolate.rs : kill-able worker process for fancy-regex jobs, one idle standby worker
│       ├── sql/
│       │   ├── mod.rs : MySQL wire client gateway
│       │   ├── password.rs : password from an environment variable, a DPAPI-sealed file, or the config value
│       │   ├── types.rs : connect options, row limits, result sets, statement summaries
│       │   ├── error.rs : client error type, timeout marker
│       │   ├── connection.rs : plain TCP connection, I/O timeout, close
│       │   ├── handshake.rs : handshake, mysql_native_password and caching_sha2_password authentication
│       │   ├── statement.rs : COM_QUERY execution, result-set reading under row limits
│       │   ├── packet.rs : packet framing, payload readers
│       │   ├── digest.rs : SHA-1 and SHA-256 for the authentication scrambles
│       │   ├── rsa.rs : RSA-OAEP encryption with the server public key
│       │   ├── random.rs : OS random source for the RSA-OAEP seed
│       │   └── fake_server.rs : test-only loopback MySQL server
│       ├── text/
│       │   ├── mod.rs : text layer gateway
│       │   ├── diff.rs : shared unified-diff engine (similar), CRLF folding
│       │   └── encoding.rs : UTF-16 and EUC-KR/CP949 decode and re-encode
│       └── web/
│           ├── mod.rs : web layer gateway
│           ├── http.rs : HTTP client, SSRF guard, RFC 3986 URL resolution
│           └── html.rs : HTML tree builder, text, markdown, links, and readability converters
├── tests/
│   ├── tool_matrix.rs : calls every catalog tool through dispatch
│   ├── isolation.rs : runaway fancy-regex kill and worker crash checks against the real binary
│   └── performance.rs : ignored local timing workload
├── bench/
│   ├── README.md : benchmark usage
│   ├── mcp-bench.ts : end-to-end stdio benchmark with correctness checks
│   ├── ab.ts : interleaved A/B comparison of two binaries
│   ├── cli-bench.ts : before/after benchmark of the CLI integration (fd, sd, git)
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
