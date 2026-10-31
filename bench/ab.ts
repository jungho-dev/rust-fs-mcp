// ab.ts
// Interleaved A/B comparison of two server binaries on the same scenario list.
// Alternating rounds keep machine load (including background apps) shared between both sides,
// so the ratio stays meaningful even when absolute latency drifts.

import { readFileSync, mkdirSync } from "node:fs";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dir, "..");
const argv = process.argv.slice(2);
const arg = (name: string, fallback: string): string => {
  const index = argv.indexOf(`--${name}`);
  return index >= 0 ? argv[index + 1] : fallback;
};

const base = arg("base", "target/release/rust-fs-mcp.exe");
const cand = arg("cand", "target/bench/release/rust-fs-mcp.exe");
const only = arg("only", "");
const rounds = Number(arg("rounds", "3"));
const resultsDir = join(root, "bench", "results");
mkdirSync(resultsDir, { recursive: true });

const runs: Array<{ label: string; report: any }> = [];

for (let round = 1; round <= rounds; round++) {
  // 짝수 라운드는 순서를 뒤집는다: 항상 같은 쪽이 나중에 돌면 머신 부하 감소나 페이지 캐시
  // 워밍이 한쪽에만 유리하게 작용해 전 시나리오가 같은 방향으로 치우친다.
  const order: Array<readonly [string, string]> =
    round % 2 === 1 ? [["base", base], ["cand", cand]] : [["cand", cand], ["base", base]];
  for (const [label, binary] of order) {
    const out = join(resultsDir, `ab-${label}-${round}.json`);
    const proc = Bun.spawnSync(
      ["bun", "bench/mcp-bench.ts", "--binary", binary, "--only", only, "--label", `${label}${round}`, "--save", out],
      { cwd: root, stdout: "pipe", stderr: "pipe" },
    );
    if (proc.exitCode !== 0 && proc.exitCode !== 2) {
      console.error(proc.stderr.toString().slice(0, 800));
      process.exit(1);
    }
    runs.push({ label, report: JSON.parse(readFileSync(out, "utf8")) });
  }
}

function median(values: number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}

const names = runs[0].report.scenarios.map((s: any) => s.name);
console.log("scenario".padEnd(26) + "base_p50  cand_p50   base_min  cand_min   ratio(p50)  ratio(min)");
for (const name of names) {
  const baseRuns = runs.filter((r) => r.label === "base").map((r) => r.report.scenarios.find((s: any) => s.name === name));
  const candRuns = runs.filter((r) => r.label === "cand").map((r) => r.report.scenarios.find((s: any) => s.name === name));
  const baseP50 = median(baseRuns.map((s) => s.p50));
  const candP50 = median(candRuns.map((s) => s.p50));
  const baseMin = Math.min(...baseRuns.map((s) => s.min));
  const candMin = Math.min(...candRuns.map((s) => s.min));
  const ratioP50 = baseP50 > 0 ? candP50 / baseP50 : 1;
  const ratioMin = baseMin > 0 ? candMin / baseMin : 1;
  console.log(
    name.padEnd(26) +
      baseP50.toFixed(3).padStart(9) +
      candP50.toFixed(3).padStart(10) +
      baseMin.toFixed(3).padStart(11) +
      candMin.toFixed(3).padStart(10) +
      `${ratioP50.toFixed(2)}x`.padStart(12) +
      `${ratioMin.toFixed(2)}x`.padStart(12),
  );
}

const failures = runs.flatMap((r) => r.report.scenarios.filter((s: any) => s.failures > 0).map((s: any) => `${r.label}:${s.name}`));
console.log(failures.length ? `\nverification failures: ${[...new Set(failures)].join(", ")}` : "\nverification: all scenarios passed on both binaries");
