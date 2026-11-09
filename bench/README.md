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

## CLI integration

### Run

1. **Commands**
    - `--base` alone runs every A-side path and marks B as skipped; add `--cand` for the full comparison.

```powershell
bun bench/cli-bench.ts --base target/ab/base.exe --rounds 3
bun bench/cli-bench.ts --base target/ab/base.exe --cand target/release/rust-fs-mcp.exe --rounds 7
```

2. **Flags**
    - `--base`: pre-integration server (21 tools), used for the A-side MCP paths.
    - `--cand`: integrated server (25 tools), used for every B path.
    - `--rounds`: timed rounds per variant after one untimed warmup (default 7).
    - `--only`: comma-separated scenario names.
    - `--save`: JSON report path (default `bench/results/cli-integration.json`).
    - `--fixtures`: fixture root (default `target/bench-fixtures/cli`); a root that covers the repository or `src` is refused.
3. **Sides**
    - A is how an agent did the task before the integration: base MCP tools, or one `fd`, `sd`, `similar`, `ast-grep`, or `git` process spawned per call.
    - B is the candidate's `path-find`, `file-replace`, `file-diff`, `ast-search`, or `dryRun` call.
    - The harness never sends `dryRun` to base: base ignores unknown keys and would write the file.

### Measurements

1. **Columns**
    - `calls`: MCP calls plus process spawns one complete task needs.
    - `out bytes`: what the agent reads, the `structuredContent` JSON of each MCP result or the stdout plus stderr of each spawn.
    - `in bytes`: what the agent writes, the JSON arguments or the shell command line.
    - `min ms`, `p50 ms`, `p95 ms`: wall clock per complete task, every call and spawn included.
    - `correct`: runs whose result matched the ground truth the harness computes on its own (Bun fs walks, JS regex, byte-exact expected files, `git diff --numstat`).
2. **Fairness**
    - Variants alternate inside every round, and even rounds reverse the order, as in `ab.ts`.
    - Edit scenarios run on a fresh copy of the fixture per run; the copy is created before the clock starts and deleted after the check.
    - The exit code is `2` when a variant expected to be correct misses its ground truth; A-side probes that document a CLI weakness (`encoding-safety` A) do not fail the run.

### Scenarios

1. **find-glob**
    - All `*.rs` in a 2,040-file tree with 40 hidden decoys: base `dir-list` plus a client filter, `fd -g`, and `path-find glob`.
2. **find-meta**
    - Files of at least 10 KiB changed within 1 day: `fd -S +10ki --changed-within 1d` and `path-find size` plus `changedWithin`.
    - Sizes 10,000 and 10,100 bytes trap a decimal `k`; mtimes are set explicitly on every run.
3. **regex-replace-batch**
    - A capture-group regex across 50 files: `sd` with 50 paths, `sd` per file, base `fs-search` plus 150 exact `file-edit` items, and one `file-replace` call.
    - Base `file-edit` has no regex mode, so the report lists that path as not possible.
4. **encoding-safety**
    - Replacements inside a UTF-16LE BOM file, an EUC-KR file, a CRLF file, and a CRLF file with a `$` anchor; each file is classified as preserved, unchanged, or corrupted.
5. **ambiguous-capture**
    - `(\w+)` with `$1_x`: the tool must reject the replacement and leave the file untouched.
6. **preview**
    - One edit previewed as a diff: base copy, edit, `similar`, restore, and remove (5 steps) against `file-edit dryRun:true`.
7. **diff**
    - Two 5,000-line files with 120 scattered edits: `git diff --no-index`, `similar`, and `file-diff`, each checked against `git diff --numstat`.
8. **ast-search**
    - `$A.unwrap()` over a snapshot of `src`: `ast-grep run --json=compact` and `ast-search`, compared as `(file, line)` sets.
9. **ast-rewrite**
    - A `$A.expect("checked")` rewrite preview over 4 files with comment and string decoys: both sides must leave the files unchanged and agree on the replacements.
10. **catalog**
    - `tools/list` bytes per session for base and candidate, the bytes of the 4 new tools, and both binary sizes.
