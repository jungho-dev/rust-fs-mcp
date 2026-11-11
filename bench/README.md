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
| `file-edit-lines:single` | one-line replacement at line 50 of a 400-row file |
| `file-replace:regex` | capture-group regex that rewrites all 400 rows, direction alternating per call |
| `file-replace:dry-run` | 400-replacement preview that returns the diff and writes nothing |
| `file-diff:pair` | 5,000-line pair with 119 changed lines |
| `path-find:glob-bigtree` | glob `f9*.txt` over the 2,000-file tree, exactly 220 files |
| `path-find:details` | files of the 200-file tree with `details`, sorted by size, first 50 |
| `fs-search:fancy-worker` | look-ahead pattern, so the item runs in the fancy-regex worker process |
| `fs-search:batch4` | 4 search items in one call: two literal searches over `src`, a regex over the tree, the 200-match file |
| `dir-create:nested` | a new three-level directory per call |
| `path-copy:file` | small file copied to a new name |
| `path-move:file` | freshly written file renamed |
| `path-remove:file` | freshly written file removed |
| `git-diff:worktree` | unstaged diff of the fixture repository |
| `git-show:head-stat` | `HEAD` with `stat: true` |
| `git-add:file` | one modified tracked file staged |
| `git-commit:file` | one staged file committed with a Conventional Commit message |
| `git-amend:message` | message-only amend of the last commit |
| `web-extract:markdown` | local 400-section HTML file to markdown |
| `web-fetch:local-html` | the same document fetched from the loopback server as markdown |
| `download-to-file:local` | 256 KiB body from the loopback server to a new file |
| `file-read:fanout32` | 32 concurrent calls through the resident workers |

Each scenario also verifies its result against ground truth computed independently in the
harness (exact file tail bytes, `totalCount`, `entryCount`, truncation flags, error envelope),
so an accuracy regression fails the run instead of passing silently.

1. **Harness setup**
    - The harness starts a loopback HTTP server on an ephemeral port for `web-fetch:local-html` and `download-to-file:local`; the SSRF guard allows loopback.
    - The git scenarios other than `git-status` run in a throwaway git repository under `target/bench-fixtures/git-repo`, recreated on every run.
    - `git-add:file`, `git-commit:file`, and `git-amend:message` check only that the call returns without an error.
    - `mysql-query` has no scenario, because it needs a database server.

## Fixtures

1. **Location**
    - The harness generates `target/bench-fixtures/`: it rewrites the small fixtures on every run and builds the 40 MB tree once, then reuses it.
    - Delete the directory to regenerate everything.
2. **Read and search**
    - A 4.5 MB 100k-line file, a 340 KB file, a 96 KB file for the truncation check, a 200-file tree, a 40 MB 2,000-file tree, and a file with exactly 200 matches.
3. **Edit and compare**
    - Edit and write targets for `file-edit:single` and `file-write:small`.
    - `replace-target.txt` and `lines-target.txt`: 400 rows each, for `file-replace:regex`, `file-replace:dry-run`, and `file-edit-lines:single`.
    - `diff-old.txt` and `diff-new.txt`: a 5,000-line pair with 119 changed lines, for `file-diff:pair`.
4. **Path operations**
    - `ops/`: a scratch directory for `dir-create`, `path-copy`, `path-move`, `path-remove`, and downloads, emptied on every run.
5. **Web**
    - `doc.html`: a 400-section HTML document of about 150 KB, read by `web-extract:markdown` and served at `/page` by the loopback server.
    - The loopback server answers every other path with a 256 KiB binary body, which `download-to-file:local` saves under `ops/`.
6. **Git**
    - `git-repo/`: 20 tracked files in one seed commit, with one appended line left unstaged in 3 of them, so `git-diff:worktree` has a body.

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
    - `--cand`: integrated server (24 tools), used for every B path.
    - `--rounds`: timed rounds per variant after one untimed warmup (default 7).
    - `--only`: comma-separated scenario names.
    - `--save`: JSON report path (default `bench/results/cli-integration.json`).
    - `--fixtures`: fixture root (default `target/bench-fixtures/cli`); a root that covers the repository or `src` is refused.
3. **Sides**
    - A is how an agent did the task before the integration: base MCP tools, or one `fd`, `sd`, or `git` process spawned per call.
    - B is the candidate's `path-find`, `file-replace`, `file-diff`, or `dryRun` call.
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
    - One edit previewed as a diff: base copy, edit, `git diff --no-index`, restore, and remove (5 steps) against `file-edit dryRun:true`.
7. **diff**
    - Two 5,000-line files with 120 scattered edits: `git diff --no-index` and `file-diff`, each checked against `git diff --numstat`.
8. **catalog**
    - `tools/list` bytes per session for base and candidate, the bytes of the 3 new tools, and both binary sizes.
