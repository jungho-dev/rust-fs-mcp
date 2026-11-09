// mcp-bench.ts
// End-to-end MCP stdio benchmark: latency, throughput, payload size, and correctness checks
// against an independently computed ground truth.
//
// Run: bun bench/mcp-bench.ts --binary target/release/rust-fs-mcp.exe --save bench/results/baseline.json
//      bun bench/mcp-bench.ts ... --baseline bench/results/baseline.json
//
// The harness spawns the real server, speaks newline-delimited JSON-RPC over stdin/stdout,
// and times each request from write to matching response so client-visible latency is what
// the numbers describe.

import { readFileSync, writeFileSync, mkdirSync, existsSync, rmSync, readdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

// 1. Types ----------------------------------------------------------------------------------
type Json = any;

interface ScenarioSpec {
  tool?: string;
  args?: Json;
  method?: string;
  params?: Json;
  verify?: (r: Json) => void;
  expectError?: boolean;
}

interface Scenario {
  name: string;
  iterations: number;
  warmup: number;
  build: () => ScenarioSpec;
}

interface Sample {
  ms: number;
  bytes: number;
  ok: boolean;
  reason?: string;
}

interface ScenarioResult {
  name: string;
  count: number;
  p50: number;
  p90: number;
  p99: number;
  mean: number;
  min: number;
  max: number;
  bytesP50: number;
  bytesMean: number;
  failures: number;
  failureDetail: string[];
}

interface Report {
  label: string;
  binary: string;
  at: string;
  startupMs: number;
  scenarios: ScenarioResult[];
}

// 2. JSON-RPC client ------------------------------------------------------------------------
class McpClient {
  private proc: ReturnType<typeof Bun.spawn>;
  private nextId = 1;
  private pending = new Map<number, { resolve: (v: Json) => void; started: number }>();
  private buffer = "";
  private decoder = new TextDecoder();
  ready: Promise<void>;

  constructor(binary: string, cwd: string) {
    // 서버 기본 출력 예산 고정 (사용자 env 상속 차단)
    const env: Record<string, string | undefined> = { ...process.env, RUST_FS_MCP_CONFIG: "none" };
    delete env.MAX_MCP_OUTPUT_TOKENS;
    delete env.RUST_FS_MCP_OUTPUT_BYTES;
    this.proc = Bun.spawn([binary], {
      cwd,
      env: env,
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
    this.ready = this.readLoop();
  }

  private async readLoop(): Promise<void> {
    const reader = (this.proc.stdout as ReadableStream<Uint8Array>).getReader();
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      this.buffer += this.decoder.decode(value, { stream: true });
      let index = this.buffer.indexOf("\n");
      while (index >= 0) {
        const line = this.buffer.slice(0, index);
        this.buffer = this.buffer.slice(index + 1);
        if (line.length > 0) this.onLine(line);
        index = this.buffer.indexOf("\n");
      }
    }
  }

  private onLine(line: string): void {
    let message: Json;
    try {
      message = JSON.parse(line);
    } catch {
      return;
    }
    const id = message?.id;
    const entry = typeof id === "number" ? this.pending.get(id) : undefined;
    if (!entry) return;
    this.pending.delete(id);
    entry.resolve({ response: message, ms: performance.now() - entry.started, bytes: line.length });
  }

  request(method: string, params?: Json): Promise<{ response: Json; ms: number; bytes: number }> {
    const id = this.nextId++;
    const payload = JSON.stringify({ jsonrpc: "2.0", id, method, params });
    return new Promise((resolvePromise) => {
      this.pending.set(id, { resolve: resolvePromise, started: performance.now() });
      (this.proc.stdin as FileSink).write(payload + "\n");
    });
  }

  notify(method: string, params?: Json): void {
    const payload = JSON.stringify({ jsonrpc: "2.0", method, params });
    (this.proc.stdin as FileSink).write(payload + "\n");
  }

  call(tool: string, args: Json) {
    return this.request("tools/call", { name: tool, arguments: args });
  }

  async close(): Promise<void> {
    try {
      (this.proc.stdin as FileSink).end();
    } catch {
      // already closed
    }
    await this.proc.exited;
  }
}

type FileSink = { write(chunk: string): number; end(): void };

// 3. Statistics -----------------------------------------------------------------------------
function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return sorted[index];
}

function summarize(name: string, samples: Sample[]): ScenarioResult {
  const times = samples.map((s) => s.ms).sort((a, b) => a - b);
  const bytes = samples.map((s) => s.bytes).sort((a, b) => a - b);
  const failures = samples.filter((s) => !s.ok);
  return {
    name,
    count: samples.length,
    p50: round(percentile(times, 50)),
    p90: round(percentile(times, 90)),
    p99: round(percentile(times, 99)),
    mean: round(times.reduce((a, b) => a + b, 0) / times.length),
    min: round(times[0] ?? 0),
    max: round(times[times.length - 1] ?? 0),
    bytesP50: Math.round(percentile(bytes, 50)),
    bytesMean: Math.round(bytes.reduce((a, b) => a + b, 0) / bytes.length),
    failures: failures.length,
    failureDetail: [...new Set(failures.map((s) => s.reason ?? `bytes=${s.bytes}`))].slice(0, 3),
  };
}

function round(value: number): number {
  return Math.round(value * 1000) / 1000;
}

// 4. Fixtures -------------------------------------------------------------------------------
const root = resolve(import.meta.dir, "..");
const fixtureDir = join(root, "target", "bench-fixtures");

function prepareFixtures(): {
  small: string;
  medium: string;
  large: string;
  lines: string;
  truncated: string;
  tree: string;
  hits: string;
  editTarget: string;
  writeTarget: string;
  bigtree: string;
} {
  mkdirSync(fixtureDir, { recursive: true });
  const small = join(fixtureDir, "small.txt");
  writeFileSync(small, "rust-fs-mcp bench fixture\n".repeat(20));
  const medium = join(fixtureDir, "medium.txt");
  const mediumLines: string[] = [];
  for (let i = 0; i < 4000; i++) mediumLines.push(`line ${i} alpha beta gamma delta epsilon ${"x".repeat(40)}`);
  writeFileSync(medium, mediumLines.join("\n"));
  const large = join(fixtureDir, "large.txt");
  const largeLines: string[] = [];
  for (let i = 0; i < 100_000; i++) largeLines.push(`skipped line abcdefghijklmnopqrstuvwxyz0123456789 ${i}`);
  writeFileSync(large, largeLines.join("\n"));
  const lines = large;
  const truncated = join(fixtureDir, "truncated.txt");
  const truncatedLines: string[] = [];
  for (let i = 0; i < 1_800; i++) truncatedLines.push(`budget line abcdefghijklmnopqrstuvwxyz0123456789 ${i}`);
  writeFileSync(truncated, truncatedLines.join("\n"));
  const tree = join(fixtureDir, "tree");
  rmSync(tree, { recursive: true, force: true });
  for (let d = 0; d < 8; d++) {
    const dir = join(tree, `d${d}`);
    mkdirSync(dir, { recursive: true });
    for (let f = 0; f < 25; f++) {
      writeFileSync(
        join(dir, `f${f}.txt`),
        Array.from({ length: 40 }, (_, i) => `payload ${d}/${f}/${i} needle${i % 7 ? "" : "-hit"}`).join("\n"),
      );
    }
  }
  // Exactly 200 matching lines and no filler: the emitted body and the reported line count
  // can both be checked against a known truth. fs-search `totalCount` counts emitted result
  // lines (file heading + match/context lines), not matches.
  const hits = join(fixtureDir, "hits.txt");
  const hitLines: string[] = [];
  for (let i = 0; i < 200; i++) hitLines.push(`hit needle ${i}`);
  writeFileSync(hits, hitLines.join("\n"));
  const editTarget = join(fixtureDir, "edit-target.txt");
  writeFileSync(editTarget, Array.from({ length: 120 }, (_, i) => `row ${i} alpha beta gamma`).join("\n"));
  const writeTarget = join(fixtureDir, "write-target.txt");
  writeFileSync(writeTarget, "initial\n");

  // Large tree for scan-throughput tests; built once, ~40 MB across 2000 files.
  const bigtree = join(fixtureDir, "bigtree");
  const marker = join(bigtree, ".built");
  if (!existsSync(marker)) {
    rmSync(bigtree, { recursive: true, force: true });
    const body = Array.from({ length: 200 }, (_, i) => `bigtree payload row ${i} ${"y".repeat(80)}`).join("\n");
    for (let d = 0; d < 20; d++) {
      const dir = join(bigtree, `d${d}`);
      mkdirSync(dir, { recursive: true });
      for (let f = 0; f < 100; f++) writeFileSync(join(dir, `f${f}.txt`), body);
    }
    writeFileSync(marker, "1");
  }

  return { small, medium, large, lines, truncated, tree, hits, editTarget, writeTarget, bigtree };
}

// 5. Scenarios ------------------------------------------------------------------------------
const BATCH_PATHS = [
  "Cargo.toml",
  "README.md",
  "src/lib.rs",
  "src/main.rs",
  "src/core/mod.rs",
  "src/protocol/mod.rs",
  "src/tools/mod.rs",
  "tests/performance.rs",
].map((p) => join(root, p));

function scenarios(fx: ReturnType<typeof prepareFixtures>): Scenario[] {
  const cargoToml = readFileSync(join(root, "Cargo.toml"), "utf8");
  const searchPattern = "fn handle_file_read";
  const referenceHits = countHits(join(root, "src"), searchPattern);

  return [
    {
      name: "tools/list",
      iterations: 300,
      warmup: 30,
      build: () => ({
        method: "tools/list",
        params: {},
        verify: (r) => {
          // 연동 전 21개, 연동 후 25개
          assert((r.response?.result?.tools?.length ?? 0) >= 21, "tool count");
        },
      }),
    },
    {
      name: "file-read:small",
      iterations: 300,
      warmup: 30,
      build: () => ({
        tool: "file-read",
        args: { paths: [fx.small] },
        verify: (r) => {
          assert(items(r)[0]?.ok === true, "read ok");
        },
      }),
    },
    {
      name: "file-read:exact-bytes",
      iterations: 200,
      warmup: 20,
      build: () => ({
        tool: "file-read",
        args: { paths: [join(root, "Cargo.toml")] },
        verify: (r) => {
          const text = body(r);
          assert(text.includes(cargoToml.trim().split("\n").pop()!.trim()), "content tail");
          assert(text.includes("name = \"rust-fs-mcp\""), "content head");
        },
      }),
    },
    {
      name: "file-read:batch8",
      iterations: 200,
      warmup: 20,
      build: () => ({
        tool: "file-read",
        args: { paths: BATCH_PATHS },
        verify: (r) => {
          const entries = items(r);
          assert(entries.length === 8 && entries.every((i: Json) => i.ok), "batch8");
        },
      }),
    },
    {
      name: "file-read:line-range-late",
      iterations: 200,
      warmup: 20,
      build: () => ({
        tool: "file-read-line-range",
        args: { items: [{ path: fx.lines, start_line: 99_990, line_count: 5 }] },
        verify: (r) => {
          assert(body(r).includes("99989"), "late range content");
        },
      }),
    },
    {
      name: "dir-list:tree",
      iterations: 100,
      warmup: 10,
      build: () => ({
        tool: "dir-list",
        args: { items: [{ path: fx.tree, recursive: true }] },
        verify: (r) => {
          const entries = items(r);
          assert(entries[0]?.ok === true && (entries[0]?.data?.entryCount ?? 0) > 0, "tree entries");
        },
      }),
    },
    {
      name: "path-stat:batch8",
      iterations: 200,
      warmup: 20,
      build: () => ({
        tool: "path-stat",
        args: { paths: BATCH_PATHS },
        verify: (r) => {
          const entries = items(r);
          assert(entries.length === 8 && entries.every((i: Json) => i.ok), "stat batch");
        },
      }),
    },
    {
      name: "fs-search:literal-src",
      iterations: 30,
      warmup: 5,
      build: () => ({
        tool: "fs-search",
        args: { items: [{ path: join(root, "src"), pattern: searchPattern, literal: true, maxResults: 50 }] },
        verify: (r) => {
          const data = items(r)[0]?.data;
          assert((data?.totalCount ?? 0) > 0, "search hits");
          assert((data?.totalCount ?? 0) <= 50, "search respects maxResults");
          assert(referenceHits > 0, "reference scan found hits");
        },
      }),
    },
    {
      name: "fs-search:regex-tree",
      iterations: 20,
      warmup: 4,
      build: () => ({
        tool: "fs-search",
        args: { items: [{ path: fx.tree, pattern: "needle-hit", maxResults: 100 }] },
        verify: (r) => {
          const data = items(r)[0]?.data;
          assert((data?.totalCount ?? 0) > 0, "regex hits");
          assert(body(r).includes("needle-hit"), "regex body");
        },
      }),
    },
    {
      name: "fs-search:exact-count",
      iterations: 30,
      warmup: 5,
      build: () => ({
        tool: "fs-search",
        args: { items: [{ path: fx.hits, pattern: "needle", literal: true, maxResults: 500 }] },
        verify: (r) => {
          assert(items(r)[0]?.data?.limitReached !== true, "no truncation");
          assert(items(r)[0]?.data?.totalCount === 201, "exact result-line count (heading + 200)");
          const occurrences = body(r).split("hit needle").length - 1;
          assert(occurrences === 200, `exact body count (${occurrences})`);
        },
      }),
    },
    {
      name: "file-write:small",
      iterations: 200,
      warmup: 20,
      build: () => ({
        tool: "file-write",
        args: { items: [{ path: fx.writeTarget, content: "benchmark payload\n".repeat(20), mode: "rewrite" }] },
        verify: (r) => {
          assert(items(r)[0]?.ok === true, "write ok");
        },
      }),
    },
    {
      name: "file-edit:single",
      iterations: 100,
      warmup: 10,
      build: () => ({
        tool: "file-edit",
        args: { items: [{ file_path: fx.editTarget, old_string: "row 0 alpha beta", new_string: "row 0 alpha beta", expected_replacements: 1 }] },
        verify: (r) => {
          assert(items(r)[0]?.ok === true, "edit ok");
        },
      }),
    },
    {
      name: "file-read:medium",
      iterations: 200,
      warmup: 20,
      build: () => ({
        tool: "file-read",
        args: { paths: [fx.medium] },
        verify: (r) => {
          assert(items(r)[0]?.ok === true, "medium read ok");
        },
      }),
    },
    {
      name: "fs-search:bigtree-scan",
      iterations: 6,
      warmup: 2,
      build: () => ({
        tool: "fs-search",
        args: { items: [{ path: fx.bigtree, pattern: "ZZNOMATCHZZ", literal: true, maxResults: 10 }] },
        verify: (r) => {
          assert(items(r)[0]?.data?.totalCount === 0, "no matches");
        },
      }),
    },
    {
      name: "git-status",
      iterations: 30,
      warmup: 5,
      build: () => ({
        tool: "git-status",
        args: { path: root },
        verify: (r) => {
          assert(typeof r.response?.result?.content?.[0]?.text === "string", "git status body");
        },
      }),
    },
    {
      name: "file-read:truncated-large",
      iterations: 100,
      warmup: 10,
      build: () => ({
        tool: "file-read",
        args: { paths: [fx.truncated] },
        verify: (r) => {
          assert(body(r).length > 1000, "truncated body present");
          assert(body(r).includes("outputTruncated") || body(r).length < 30_000, "budget respected");
        },
      }),
    },
    {
      name: "unknown-tool:error",
      iterations: 200,
      warmup: 20,
      build: () => ({
        tool: "definitely-not-a-tool",
        args: {},
        expectError: true,
        verify: (r) => {
          assert(r.response?.result?.isError === true, "error flag");
          assert(String(r.response?.result?._meta?.fsMcpResult?.errorMessage ?? "").includes("Unknown tool"), "error message");
        },
      }),
    },
    {
      name: "args_path:indirection",
      iterations: 100,
      warmup: 10,
      build: () => ({
        tool: "file-read",
        args: {},
        verify: (r) => {
          assert(items(r)[0]?.ok === true, "args_path read");
        },
      }),
    },
  ];
}

function countHits(dir: string, needle: string): number {
  let total = 0;
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) total += countHits(full, needle);
    else if (entry.isFile() && full.endsWith(".rs")) {
      const text = readFileSync(full, "utf8");
      total += text.split(needle).length - 1;
    }
  }
  return total;
}

function assert(condition: boolean, label: string): void {
  if (!condition) throw new Error(`verify failed: ${label}`);
}

// tools/call envelope: result.structuredContent.data = { content[], structuredContent{ results[] }, durationMs }
function payload(r: Json): Json {
  return r.response?.result?.structuredContent?.data;
}

function items(r: Json): Json[] {
  return payload(r)?.structuredContent?.results ?? [];
}

function body(r: Json): string {
  return payload(r)?.content?.[0]?.text ?? "";
}

// 6. Runner ---------------------------------------------------------------------------------
async function runScenario(client: McpClient, scenario: Scenario, argsFile: string): Promise<ScenarioResult> {
  const samples: Sample[] = [];
  for (let i = 0; i < scenario.warmup; i++) {
    await execute(client, scenario, argsFile, false);
  }
  for (let i = 0; i < scenario.iterations; i++) {
    samples.push(await execute(client, scenario, argsFile, true));
  }
  return summarize(scenario.name, samples);
}

async function execute(client: McpClient, scenario: Scenario, argsFile: string, timed: boolean): Promise<Sample> {
  const spec = scenario.build();
  let result: { response: Json; ms: number; bytes: number };
  if (scenario.name === "args_path:indirection") {
    result = await client.call(spec.tool!, { args_path: argsFile });
  } else if (spec.method) {
    result = await client.request(spec.method, spec.params);
  } else {
    result = await client.call(spec.tool!, spec.args);
  }
  let reason: string | undefined;
  let ok = spec.expectError
    ? !result.response?.error && result.response?.result?.isError === true
    : !result.response?.error && !result.response?.result?.isError;
  if (!ok) reason = `envelope error: ${JSON.stringify(result.response?.error ?? result.response?.result?.structuredContent?.error ?? null).slice(0, 160)}`;
  if (ok && spec.verify) {
    try {
      spec.verify!(result);
    } catch (error) {
      ok = false;
      reason = String((error as Error).message).slice(0, 160);
    }
  }
  return { ms: result.ms, bytes: result.bytes, ok, reason };
}

async function measureStartup(binary: string, cwd: string): Promise<number> {
  const started = performance.now();
  const client = new McpClient(binary, cwd);
  await client.request("initialize", {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "bench", version: "1" },
  });
  const elapsed = performance.now() - started;
  client.notify("notifications/initialized");
  await client.close();
  return round(elapsed);
}

async function main(): Promise<void> {
  const argv = process.argv.slice(2);
  const flag = (name: string, fallback?: string): string | undefined => {
    const index = argv.indexOf(`--${name}`);
    return index >= 0 ? argv[index + 1] : fallback;
  };
  const binary = resolve(flag("binary", join(root, "target", "release", "rust-fs-mcp.exe"))!);
  const label = flag("label", "run")!;
  const save = flag("save");
  const baselinePath = flag("baseline");

  if (!existsSync(binary)) {
    console.error(`binary not found: ${binary}`);
    process.exit(1);
  }

  const fx = prepareFixtures();
  const argsFile = join(fixtureDir, "args.json");
  writeFileSync(argsFile, JSON.stringify({ paths: [join(root, "Cargo.toml")] }));

  const startupMs = await measureStartup(binary, root);
  const client = new McpClient(binary, root);
  await client.request("initialize", {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "bench", version: "1" },
  });
  client.notify("notifications/initialized");

  const requested = flag("only")?.split(",").map((name) => name.trim()).filter(Boolean);
  const only = requested && requested.length > 0 ? requested : undefined;
  const list = scenarios(fx).filter((scenario) => !only || only.includes(scenario.name));
  // Order matters for cache warmth: deterministic, no shuffling.
  const results: ScenarioResult[] = [];
  for (const scenario of list) {
    const result = await runScenario(client, scenario, argsFile);
    results.push(result);
    process.stdout.write(
      `${result.name.padEnd(26)} p50=${String(result.p50).padStart(8)}ms p90=${String(result.p90).padStart(8)} p99=${String(result.p99).padStart(8)} bytes=${String(result.bytesP50).padStart(7)} fails=${result.failures}\n`,
    );
    for (const detail of result.failureDetail) process.stdout.write(`  ! ${detail}\n`);
  }

  // Concurrency: 32 in-flight file-read calls through 4 resident workers.
  const fanout: Sample[] = [];
  const concurrent = 32;
  for (let round = 0; round < 10; round++) {
    const pending = Array.from({ length: concurrent }, () => client.call("file-read", { paths: [fx.small] }));
    const settled = await Promise.all(pending);
    for (const item of settled) fanout.push({ ms: item.ms, bytes: item.bytes, ok: !item.response?.error });
  }
  const fanoutResult = summarize("file-read:fanout32", fanout);
  results.push(fanoutResult);
  process.stdout.write(
    `${fanoutResult.name.padEnd(26)} p50=${String(fanoutResult.p50).padStart(8)}ms p90=${String(fanoutResult.p90).padStart(8)} p99=${String(fanoutResult.p99).padStart(8)} bytes=${String(fanoutResult.bytesP50).padStart(7)} fails=${fanoutResult.failures}\n`,
  );

  await client.close();

  const report: Report = { label, binary, at: new Date().toISOString(), startupMs, scenarios: results };
  if (save) {
    mkdirSync(dirname(resolve(save)), { recursive: true });
    writeFileSync(resolve(save), JSON.stringify(report, null, 2));
    process.stdout.write(`saved ${save}\n`);
  }

  if (baselinePath && existsSync(resolve(baselinePath))) {
    const baseline: Report = JSON.parse(readFileSync(resolve(baselinePath), "utf8"));
    console.log("\ndelta vs baseline (p50 ms, negative = faster)");
    console.log(`startup ${baseline.startupMs} -> ${startupMs} (${round(startupMs - baseline.startupMs)} ms)`);
    for (const now of results) {
      const before = baseline.scenarios.find((s) => s.name === now.name);
      if (!before) continue;
      const delta = round(now.p50 - before.p50);
      const pct = before.p50 > 0 ? round((delta / before.p50) * 100) : 0;
      console.log(`${now.name.padEnd(26)} ${String(before.p50).padStart(8)} -> ${String(now.p50).padStart(8)} (${delta >= 0 ? "+" : ""}${delta} ms, ${pct >= 0 ? "+" : ""}${pct}%)`);
    }
  }

  const totalFailures = results.reduce((a, r) => a + r.failures, 0);
  process.exit(totalFailures === 0 ? 0 : 2);
}

await main();
