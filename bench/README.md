# bench

End-to-end benchmark for the stdio MCP server. Every number is client-visible latency: the
harness spawns the real binary, performs the `initialize` handshake, then times each
`tools/call` from the stdin write to the matching stdout line.

## Run

```powershell
cargo build --release
bun bench/mcp-bench.ts --binary target/release/rust-fs-mcp.exe --save bench/results/baseline.json
```

| Flag | Meaning |
| --- | --- |
| `--binary` | server executable (default `target/release/rust-fs-mcp.exe`) |
| `--label` | tag written into the report |
| `--save` | write the JSON report to this path |
| `--baseline` | compare the run against an earlier saved report |
| `--only` | comma-separated scenario names |

## A/B

Absolute latency moves with machine load, so a before/after pair taken minutes apart can
invert. `ab.ts` alternates the two binaries in the same time window and reverses the order
on even rounds, so a warming page cache or a decaying background load cannot favor one side:

```powershell
bun bench/ab.ts --base target/release/rust-fs-mcp.exe --cand target/bench/release/rust-fs-mcp.exe --rounds 4
```

The `ratio(p50)` and `ratio(min)` columns are `candidate / baseline`; below `1.00` is faster.
Untouched scenarios act as the control: they must stay near `1.00x`, otherwise the run is
biased and should be repeated.

## Scenarios

| Scenario | What it exercises |
| --- | --- |
| `tools/list` | catalog splice path, no re-serialization |
| `file-read:small` / `:exact-bytes` / `:medium` | single-file read, byte fidelity, 340 KB body |
| `file-read:batch8` | 8-file batch through the read worker plan |
| `file-read:line-range-late` | 5 lines at line 99,990 of a 4.5 MB file |
| `file-read:truncated-large` | 96 KB fixture cut by the 30,000-byte output budget |
| `dir-list:tree` | recursive listing of a 200-entry tree |
| `path-stat:batch8` | 8 metadata probes |
| `fs-search:literal-src` | literal search over `src` |
| `fs-search:regex-tree` | regex search over the 200-file tree |
| `fs-search:exact-count` | single file with exactly 200 matches (accuracy) |
| `fs-search:bigtree-scan` | 40 MB / 2,000-file scan with no matches |
| `file-write:small` | atomic rewrite of a small file |
| `file-edit:single` | exact block replacement |
| `git-status` | git CLI round trip |
| `unknown-tool:error` | error envelope |
| `args_path:indirection` | `args_path` argument indirection |
| `file-read:fanout32` | 32 concurrent calls through the resident workers |

Each scenario also verifies its result against ground truth computed independently in the
harness (exact file tail bytes, `totalCount`, `entryCount`, truncation flags, error envelope),
so an accuracy regression fails the run instead of passing silently.

## Fixtures

`target/bench-fixtures/` is generated on first use: a 4.5 MB 100k-line file, a 340 KB file, a
200-file tree, a 40 MB 2,000-file tree, a file with exactly 200 matches, and edit/write targets.
Delete the directory to regenerate.

## Diagnostics

`probe.ts` and `probe-search.ts` dump raw JSON-RPC result shapes for a few tools, which is how
the scenario verifiers were written against the real envelope instead of a guess.
