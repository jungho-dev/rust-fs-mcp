/**
 * [cli-bench.ts - CLI 연동 전후 비교 벤치]
 *
 * - 실행: bun bench/cli-bench.ts --base target/ab/base.exe [--cand <exe>] [--rounds 7] [--only a,b] [--save <json>] [--fixtures <dir>]
 * - A: 연동 전 경로 (base 서버 21개 도구, 호출마다 spawn 하는 fd, sd, git)
 * - B: 후보 서버 24개 도구, --cand 없으면 skipped
 * - 측정: 작업당 wall clock (min, p50, p95), 독립 정답 대조, 에이전트가 읽는 출력 바이트, 호출 수
 * - 라운드마다 A, B 교대, 짝수 라운드 순서 반전 (ab.ts 와 동일)
 * - 픽스처: --fixtures 아래만 생성, 편집 시나리오는 실행마다 새 사본
 * - base 에는 dryRun 전송 금지 (모르는 키 무시 후 실제 기록)
 */

import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

// 1. Types --------------------------------------------------------------------
type Side = "A" | "B";
type Need = "base" | "cand" | "none";

interface RpcReply {
    message: unknown;
    ms: number;
    bytes: number;
}

interface ToolItem {
    ok: boolean;
    data: Record<string, unknown>;
    error: string;
}

interface ToolReply {
    isError: boolean;
    errorText: string;
    text: string;
    items: ToolItem[];
    structured: unknown;
}

interface CliReply {
    exitCode: number;
    stdout: Buffer;
    stderr: string;
}

interface Verdict {
    ok: boolean;
    note: string;
}

interface RunContext {
    base: McpClient | null;
    cand: McpClient | null;
    dir: string;
    meter: TaskMeter;
}

interface Variant {
    id: string;
    side: Side;
    label: string;
    need: Need;
    clis: string[];
    expectCorrect: boolean;
    possible: boolean;
    reason: string;
    exec: (ctx: RunContext) => Promise<() => Verdict>;
}

interface Scenario {
    name: string;
    title: string;
    mutates: boolean;
    prepare: (dir: string) => Record<string, unknown>;
    variants: Variant[];
}

interface RunSample {
    ms: number;
    calls: number;
    outBytes: number;
    inBytes: number;
    ok: boolean;
    note: string;
}

interface VariantReport {
    id: string;
    side: Side;
    label: string;
    status: "ran" | "skipped" | "not-possible";
    reason: string;
    expectCorrect: boolean;
    runs: number;
    correctRuns: number;
    calls: number;
    outBytesP50: number;
    inBytesP50: number;
    minMs: number;
    p50Ms: number;
    p95Ms: number;
    notes: string[];
}

interface ScenarioReport {
    name: string;
    title: string;
    facts: Record<string, unknown>;
    variants: VariantReport[];
}

// 2. Arguments ----------------------------------------------------------------
const ROOT = resolve(import.meta.dir, "..");
const ARGV = process.argv.slice(2);

function readFlag(name: string, fallback: string): string {
    const index = ARGV.indexOf(`--${name}`);
    if (index < 0) {
        return fallback;
    }
    const flagText = ARGV[index + 1];
    if (flagText === undefined || flagText.startsWith("--")) {
        console.error(`--${name} needs a value`);
        process.exit(1);
    }
    return flagText;
}

const BASE_PATH = readFlag("base", "");
const CAND_PATH = readFlag("cand", "");
const ROUNDS = Number(readFlag("rounds", "7"));
const ONLY_NAMES = readFlag("only", "").split(",").map((name) => name.trim()).filter(Boolean);
const SAVE_PATH = resolve(readFlag("save", join(ROOT, "bench", "results", "cli-integration.json")));
const FIXTURES = resolve(readFlag("fixtures", join(ROOT, "target", "bench-fixtures", "cli")));
const CALL_TIMEOUT_MS = 60_000;
const LIST_BATCH = 8;

// 3. JSON-RPC client ----------------------------------------------------------
type FileSink = { write(chunk: string): number; end(): void };

class McpClient {
    private proc: ReturnType<typeof Bun.spawn>;
    private nextId = 1;
    private pending = new Map<number, { settle: (reply: RpcReply) => void; started: number }>();
    private buffer = "";
    private decoder = new TextDecoder();
    readonly binary: string;

    constructor(binary: string) {
        this.binary = binary;
        this.proc = Bun.spawn([binary], {
            cwd: ROOT,
            env: {
                ...process.env,
                RUST_FS_MCP_CONFIG: "none",
            },
            stdin: "pipe",
            stdout: "pipe",
            stderr: "ignore",
        });
        void this.readStdout();
    }

    private async readStdout(): Promise<void> {
        const reader = (this.proc.stdout as ReadableStream<Uint8Array>).getReader();
        while (true) {
            const chunk = await reader.read();
            if (chunk.done) {
                break;
            }
            this.buffer += this.decoder.decode(chunk.value, { stream: true });
            let index = this.buffer.indexOf("\n");
            while (index >= 0) {
                const line = this.buffer.slice(0, index);
                this.buffer = this.buffer.slice(index + 1);
                if (line.length > 0) {
                    this.settleLine(line);
                }
                index = this.buffer.indexOf("\n");
            }
        }
    }

    private settleLine(line: string): void {
        let message: unknown;
        try {
            message = JSON.parse(line);
        }
        catch {
            return;
        }
        const id = pick(message, "id");
        const entry = typeof id === "number" ? this.pending.get(id) : undefined;
        if (!entry || typeof id !== "number") {
            return;
        }
        this.pending.delete(id);
        entry.settle({
            message: message,
            ms: performance.now() - entry.started,
            bytes: Buffer.byteLength(line),
        });
    }

    request(method: string, params: unknown): Promise<RpcReply> {
        const id = this.nextId++;
        const payload = JSON.stringify({
            jsonrpc: "2.0",
            id: id,
            method: method,
            params: params,
        });
        return new Promise((settle, fail) => {
            const timer = setTimeout(() => {
                this.pending.delete(id);
                fail(new Error(`${method} timed out after ${CALL_TIMEOUT_MS} ms`));
            }, CALL_TIMEOUT_MS);
            this.pending.set(id, {
                settle: (reply) => {
                    clearTimeout(timer);
                    settle(reply);
                },
                started: performance.now(),
            });
            (this.proc.stdin as FileSink).write(payload + "\n");
        });
    }

    async initialize(): Promise<void> {
        await this.request("initialize", {
            protocolVersion: "2025-06-18",
            capabilities: {},
            clientInfo: {
                name: "cli-bench",
                version: "1",
            },
        });
        (this.proc.stdin as FileSink).write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
    }

    async close(): Promise<void> {
        try {
            (this.proc.stdin as FileSink).end();
        }
        catch {
            // 이미 닫힘
        }
        await this.proc.exited;
    }
}

// 4. Call meter ---------------------------------------------------------------
const CLI_PATHS = new Map<string, string>();

class TaskMeter {
    calls = 0;
    outBytes = 0;
    inBytes = 0;

    async callTool(client: McpClient | null, tool: string, args: Record<string, unknown>): Promise<ToolReply> {
        if (!client) {
            throw new Error(`no server for ${tool}`);
        }
        const argText = JSON.stringify(args);
        const reply = await client.request("tools/call", {
            name: tool,
            arguments: args,
        });
        this.calls += 1;
        this.inBytes += Buffer.byteLength(argText);
        this.outBytes += Buffer.byteLength(agentView(reply.message));
        return parseToolReply(reply.message);
    }

    async listTools(client: McpClient | null): Promise<unknown> {
        if (!client) {
            throw new Error("no server for tools/list");
        }
        const reply = await client.request("tools/list", {});
        this.calls += 1;
        this.outBytes += Buffer.byteLength(JSON.stringify(pick(reply.message, "result") ?? null));
        return reply.message;
    }

    runCli(name: string, args: string[], cwd?: string): CliReply {
        const exe = CLI_PATHS.get(name);
        if (!exe) {
            throw new Error(`${name} not on PATH`);
        }
        const proc = Bun.spawnSync([exe, ...args], {
            cwd: cwd ?? ROOT,
            env: {
                ...process.env,
                NO_COLOR: "1",
            },
            stdin: "ignore",
            stdout: "pipe",
            stderr: "pipe",
            timeout: CALL_TIMEOUT_MS,
        });
        const stdout = Buffer.from(proc.stdout ?? new Uint8Array());
        const stderr = Buffer.from(proc.stderr ?? new Uint8Array());
        this.calls += 1;
        this.inBytes += Buffer.byteLength([name, ...args.map(quoteShellArg)].join(" "));
        this.outBytes += stdout.length + stderr.length;
        return {
            exitCode: proc.exitCode ?? -1,
            stdout: stdout,
            stderr: stderr.toString("utf8"),
        };
    }
}

function quoteShellArg(text: string): string {
    return /^[\w./:=+-]+$/.test(text) ? text : `'${text.replace(/'/g, "'\\''")}'`;
}

// 5. Reply parsing ------------------------------------------------------------
function pick(source: unknown, ...keys: Array<string | number>): unknown {
    let cursor = source;
    for (const key of keys) {
        if (cursor === null || typeof cursor !== "object") {
            return undefined;
        }
        cursor = (cursor as Record<string | number, unknown>)[key];
    }
    return cursor;
}

function asRecord(source: unknown): Record<string, unknown> {
    return source !== null && typeof source === "object" && !Array.isArray(source) ? (source as Record<string, unknown>) : {};
}

function agentView(message: unknown): string {
    const result = pick(message, "result");
    const structured = pick(result, "structuredContent");
    if (structured !== undefined) {
        return JSON.stringify(structured);
    }
    return JSON.stringify(pick(result, "content") ?? pick(message, "error") ?? null);
}

function parseToolReply(message: unknown): ToolReply {
    const rpcError = pick(message, "error");
    if (rpcError !== undefined) {
        return {
            isError: true,
            errorText: JSON.stringify(rpcError),
            text: "",
            items: [],
            structured: undefined,
        };
    }
    const result = pick(message, "result");
    const inner = pick(result, "structuredContent", "data");
    const contents = pick(inner, "content");
    const texts = Array.isArray(contents) ? contents.map((part) => String(pick(part, "text") ?? "")) : [];
    const rawItems = pick(inner, "structuredContent", "results");
    const items = Array.isArray(rawItems)
        ? rawItems.map((entry) => ({
            ok: pick(entry, "ok") === true,
            data: asRecord(pick(entry, "data")),
            error: JSON.stringify(pick(entry, "error") ?? ""),
        }))
        : [];
    const isError = pick(result, "isError") === true;
    const outerTexts = pick(result, "content");
    const outerText = Array.isArray(outerTexts) ? outerTexts.map((part) => String(pick(part, "text") ?? "")).join("\n") : "";
    const errorText = String(pick(result, "_meta", "fsMcpResult", "errorMessage") ?? (isError ? outerText : ""));
    return {
        isError: isError,
        errorText: errorText,
        text: texts.join("\n"),
        items: items,
        structured: pick(inner, "structuredContent"),
    };
}

function findKey(source: unknown, key: string): unknown {
    if (source === null || typeof source !== "object") {
        return undefined;
    }
    if (!Array.isArray(source) && key in source) {
        return (source as Record<string, unknown>)[key];
    }
    for (const child of Object.values(source as Record<string, unknown>)) {
        const found = findKey(child, key);
        if (found !== undefined) {
            return found;
        }
    }
    return undefined;
}

function toSlash(path: string): string {
    return path.replace(/\\/g, "/");
}

function relativeTo(root: string, path: string): string {
    const rootSlash = toSlash(root).replace(/\/$/, "");
    const pathSlash = toSlash(path.trim());
    if (pathSlash.toLowerCase().startsWith(rootSlash.toLowerCase() + "/")) {
        return pathSlash.slice(rootSlash.length + 1);
    }
    return pathSlash.replace(/^\.\//, "");
}

function extractPaths(text: string, root: string, suffix: string): string[] {
    const found = new Set<string>();
    for (const rawLine of text.split(/\r?\n/)) {
        const token = rawLine.trim().split(/\t| {2,}/)[0] ?? "";
        if (token.endsWith(suffix)) {
            found.add(relativeTo(root, token));
        }
    }
    return [...found].sort();
}

interface DiffLines {
    added: string[];
    removed: string[];
    hunks: number;
}

function parseUnifiedDiff(text: string): DiffLines {
    const parsed: DiffLines = {
        added: [],
        removed: [],
        hunks: 0,
    };
    let oldLeft = 0;
    let newLeft = 0;
    for (const line of text.split("\n").map((raw) => raw.replace(/\r$/, ""))) {
        const header = /^@@ -\d+(?:,(\d+))? \+\d+(?:,(\d+))? @@/.exec(line);
        if (header) {
            parsed.hunks += 1;
            oldLeft = Number(header[1] ?? "1");
            newLeft = Number(header[2] ?? "1");
            continue;
        }
        if (oldLeft <= 0 && newLeft <= 0) {
            continue;
        }
        if (line.startsWith("+")) {
            parsed.added.push(line.slice(1));
            newLeft -= 1;
        }
        else if (line.startsWith("-")) {
            parsed.removed.push(line.slice(1));
            oldLeft -= 1;
        }
        else if (line.startsWith(" ") || line === "") {
            oldLeft -= 1;
            newLeft -= 1;
        }
    }
    return parsed;
}

function compareSets(label: string, actual: string[], expected: string[]): Verdict {
    const actualSet = new Set(actual);
    const expectedSet = new Set(expected);
    const missing = expected.filter((entry) => !actualSet.has(entry));
    const extra = actual.filter((entry) => !expectedSet.has(entry));
    if (missing.length === 0 && extra.length === 0) {
        return {
            ok: true,
            note: `${label}: ${expected.length}/${expected.length}`,
        };
    }
    return {
        ok: false,
        note: `${label}: ${actualSet.size} got vs ${expected.length} expected, missing ${missing.length} [${missing.slice(0, 2).join(", ")}], extra ${extra.length} [${extra.slice(0, 2).join(", ")}]`,
    };
}

// 6. Encoding helpers ---------------------------------------------------------
let eucKrTable: Map<string, number[]> | null = null;

function encodeEucKr(text: string): Buffer {
    if (!eucKrTable) {
        eucKrTable = new Map();
        const decoder = new TextDecoder("euc-kr");
        const trailRanges: Array<[number, number, number, number]> = [
            [0xa1, 0xfe, 0xa1, 0xfe],
            [0x81, 0xfe, 0x41, 0x5a],
            [0x81, 0xfe, 0x61, 0x7a],
            [0x81, 0xfe, 0x81, 0xfe],
        ];
        for (const [leadLow, leadHigh, trailLow, trailHigh] of trailRanges) {
            for (let lead = leadLow; lead <= leadHigh; lead++) {
                for (let trail = trailLow; trail <= trailHigh; trail++) {
                    const char = decoder.decode(new Uint8Array([lead, trail]));
                    if (char.length === 1 && char.codePointAt(0) !== 0xfffd && !eucKrTable.has(char)) {
                        eucKrTable.set(char, [lead, trail]);
                    }
                }
            }
        }
    }
    const bytes: number[] = [];
    for (const char of text) {
        const code = char.codePointAt(0) ?? 0;
        if (code < 0x80) {
            bytes.push(code);
            continue;
        }
        const pair = eucKrTable.get(char);
        if (!pair) {
            throw new Error(`no EUC-KR code for ${char}`);
        }
        bytes.push(...pair);
    }
    return Buffer.from(bytes);
}

function encodeUtf16Le(text: string): Buffer {
    return Buffer.concat([Buffer.from([0xff, 0xfe]), Buffer.from(text, "utf16le")]);
}

// 7. Fixture helpers ----------------------------------------------------------
function resetDir(dir: string): void {
    rmSync(dir, {
        recursive: true,
        force: true,
    });
    mkdirSync(dir, { recursive: true });
}

function walkFiles(root: string, prefix = ""): string[] {
    const files: string[] = [];
    for (const entry of readdirSync(join(root, prefix), { withFileTypes: true })) {
        const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
        if (entry.isDirectory()) {
            files.push(...walkFiles(root, rel));
        }
        else if (entry.isFile()) {
            files.push(rel);
        }
    }
    return files;
}

function seededRandom(seed: number): () => number {
    let state = seed >>> 0;
    return () => {
        state = (state + 0x6d2b79f5) >>> 0;
        let mixed = Math.imul(state ^ (state >>> 15), 1 | state);
        mixed = (mixed + Math.imul(mixed ^ (mixed >>> 7), 61 | mixed)) ^ mixed;
        return ((mixed ^ (mixed >>> 14)) >>> 0) / 4294967296;
    };
}

function readBytes(path: string): Buffer {
    return existsSync(path) ? readFileSync(path) : Buffer.alloc(0);
}

// 8. Scenarios: find ----------------------------------------------------------
function variant(spec: Partial<Variant> & Pick<Variant, "id" | "side" | "label" | "need">): Variant {
    return {
        clis: [],
        expectCorrect: true,
        possible: true,
        reason: "",
        exec: async () => () => ({
            ok: false,
            note: "no exec",
        }),
        ...spec,
    };
}

async function listTreeWithDirList(meter: TaskMeter, client: McpClient | null, root: string): Promise<string[]> {
    const files: string[] = [];
    const deep: string[] = [root];
    const shallow: string[] = [];
    while (deep.length > 0 || shallow.length > 0) {
        const useDeep = deep.length > 0;
        const batch = (useDeep ? deep : shallow).splice(0, LIST_BATCH);
        const reply = await meter.callTool(client, "dir-list", {
            items: batch.map((path) => ({
                path: path,
                depth: useDeep ? 64 : 1,
            })),
        });
        const sections = splitListing(reply.text);
        batch.forEach((dir, index) => {
            const entries = sections.get(toSlash(dir).toLowerCase()) ?? [];
            const declared = reply.items[index]?.data.entryCount;
            const complete = reply.items[index]?.data.truncated !== true && typeof declared === "number" && declared === entries.length;
            if (useDeep && !complete) {
                shallow.push(dir);
                return;
            }
            if (!complete) {
                throw new Error(`dir-list depth 1 still incomplete for ${dir}`);
            }
            for (const entry of entries) {
                if (entry.endsWith("/")) {
                    if (!useDeep) {
                        deep.push(`${dir}/${entry.slice(0, -1)}`);
                    }
                    continue;
                }
                files.push(relativeTo(root, `${dir}/${entry}`));
            }
        });
    }
    return files;
}

function splitListing(text: string): Map<string, string[]> {
    const sections = new Map<string, string[]>();
    let current: string[] | null = null;
    for (const line of text.split(/\r?\n/)) {
        if (/^- \[\d+\]/.test(line) || line.startsWith("dir-list:") || line.trim() === "") {
            current = null;
            continue;
        }
        const heading = /^([A-Za-z]:[\\/].*|\/.*):$/.exec(line);
        if (heading?.[1]) {
            current = [];
            sections.set(toSlash(heading[1]).toLowerCase(), current);
            continue;
        }
        current?.push(line);
    }
    return sections;
}

function buildFindGlob(): Scenario {
    let truth: string[] = [];
    return {
        name: "find-glob",
        title: "all *.rs in a 2,000-file tree",
        mutates: false,
        prepare: (dir) => {
            const extensions = ["rs", "txt", "md", "rs.txt"];
            for (let top = 0; top < 20; top++) {
                const topName = `d${String(top).padStart(2, "0")}`;
                writeFileSync(join(dir, `.${topName}.hidden.rs`), "hidden\n");
                mkdirSync(join(dir, topName, ".cache"), { recursive: true });
                writeFileSync(join(dir, topName, ".cache", "skip.rs"), "hidden\n");
                for (let sub = 0; sub < 4; sub++) {
                    const subDir = join(dir, topName, `s${sub}`);
                    mkdirSync(subDir, { recursive: true });
                    for (let file = 0; file < 25; file++) {
                        writeFileSync(join(subDir, `f${String(file).padStart(2, "0")}.${extensions[file % 4]}`), `${topName}/${sub}/${file}\n`);
                    }
                }
            }
            truth = walkFiles(dir).filter((rel) => rel.endsWith(".rs") && !rel.split("/").some((part) => part.startsWith("."))).sort();
            return {
                files: walkFiles(dir).length,
                visibleRs: truth.length,
                hiddenDecoys: 40,
            };
        },
        variants: [
            variant({
                id: "A1",
                side: "A",
                label: "base dir-list + client filter",
                need: "base",
                exec: async (ctx) => {
                    const files = await listTreeWithDirList(ctx.meter, ctx.base, ctx.dir);
                    return () => compareSets("rs files", files.filter((rel) => rel.endsWith(".rs") && !rel.split("/").some((part) => part.startsWith("."))), truth);
                },
            }),
            variant({
                id: "A2",
                side: "A",
                label: "fd -g *.rs spawn",
                need: "none",
                clis: ["fd"],
                exec: async (ctx) => {
                    const reply = ctx.meter.runCli("fd", ["-g", "*.rs", "-t", "f", ctx.dir]);
                    return () => compareSets(`rs files (exit ${reply.exitCode})`, extractPaths(reply.stdout.toString("utf8"), ctx.dir, ".rs"), truth);
                },
            }),
            variant({
                id: "B",
                side: "B",
                label: "path-find glob",
                need: "cand",
                exec: async (ctx) => {
                    const reply = await ctx.meter.callTool(ctx.cand, "path-find", {
                        items: [
                            {
                                path: ctx.dir,
                                glob: "*.rs",
                                type: "file",
                            },
                        ],
                    });
                    return () => {
                        if (reply.isError || reply.items[0]?.ok === false) {
                            return {
                                ok: false,
                                note: `error: ${reply.errorText || reply.items[0]?.error}`.slice(0, 200),
                            };
                        }
                        const verdict = compareSets("rs files", extractPaths(reply.text, ctx.dir, ".rs"), truth);
                        const partial = findKey(reply.structured, "limitReached") === true;
                        return {
                            ok: verdict.ok && !partial,
                            note: partial ? `${verdict.note}, limitReached` : verdict.note,
                        };
                    };
                },
            }),
        ],
    };
}

function buildFindMeta(): Scenario {
    let truth: string[] = [];
    return {
        name: "find-meta",
        title: "files of at least 10 KiB changed within 1 day",
        mutates: false,
        prepare: (dir) => {
            const sizes = [500, 5_000, 10_000, 10_100, 10_239, 10_240, 10_241, 20_000, 50_000];
            const ageHours = [1, 20, 23, 25, 30, 72, 240];
            const now = Date.now();
            const expected: string[] = [];
            for (let index = 0; index < 126; index++) {
                const rel = `g${index % 4}/m${String(index).padStart(3, "0")}.dat`;
                const path = join(dir, rel);
                mkdirSync(dirname(path), { recursive: true });
                const size = sizes[index % sizes.length] ?? 0;
                const age = ageHours[index % ageHours.length] ?? 0;
                writeFileSync(path, Buffer.alloc(size, 0x61));
                const stamp = new Date(now - age * 3_600_000);
                utimesSync(path, stamp, stamp);
                if (size >= 10_240 && age < 24) {
                    expected.push(rel);
                }
            }
            truth = expected.sort();
            return {
                files: 126,
                expected: truth.length,
                rule: "size >= 10240 bytes (+10ki) and mtime within 24h; 10000/10100 trap a decimal k",
            };
        },
        variants: [
            variant({
                id: "A",
                side: "A",
                label: "fd -S +10ki --changed-within 1d",
                need: "none",
                clis: ["fd"],
                exec: async (ctx) => {
                    const reply = ctx.meter.runCli("fd", ["-t", "f", "-S", "+10ki", "--changed-within", "1d", ".", ctx.dir]);
                    return () => compareSets(`matches (exit ${reply.exitCode})`, extractPaths(reply.stdout.toString("utf8"), ctx.dir, ".dat"), truth);
                },
            }),
            variant({
                id: "B",
                side: "B",
                label: "path-find size + changedWithin",
                need: "cand",
                exec: async (ctx) => {
                    const reply = await ctx.meter.callTool(ctx.cand, "path-find", {
                        items: [
                            {
                                path: ctx.dir,
                                type: "file",
                                size: "+10ki",
                                changedWithin: "1d",
                            },
                        ],
                    });
                    return () => {
                        if (reply.isError || reply.items[0]?.ok === false) {
                            return {
                                ok: false,
                                note: `error: ${reply.errorText || reply.items[0]?.error}`.slice(0, 200),
                            };
                        }
                        return compareSets("matches", extractPaths(reply.text, ctx.dir, ".dat"), truth);
                    };
                },
            }),
        ],
    };
}

// 9. Scenarios: replace -------------------------------------------------------
const REPLACE_PATTERN = "fn (\\w+)_old\\(";
const REPLACE_WITH = "fn ${1}_new(";
const REPLACE_REGEX = /fn (\w+)_old\(/g;

function buildRegexReplace(): Scenario {
    const expected = new Map<string, string>();
    let names: string[] = [];
    const checkFiles = (dir: string): Verdict => {
        const wrong = names.filter((name) => readFileSync(join(dir, name), "utf8") !== expected.get(name));
        return {
            ok: wrong.length === 0,
            note: wrong.length === 0 ? `${names.length}/${names.length} files byte-exact` : `${wrong.length} files differ, first ${wrong[0]}`,
        };
    };
    return {
        name: "regex-replace-batch",
        title: "capture-group regex replace across 50 files",
        mutates: true,
        prepare: (dir) => {
            expected.clear();
            names = [];
            for (let index = 0; index < 50; index++) {
                const name = `r${String(index).padStart(2, "0")}.rs`;
                const body = [
                    `// module ${index}`,
                    `fn alpha_${index}_old(x: u32) -> u32 { x + ${index} }`,
                    `fn beta_${index}_old(y: u32) -> u32 { alpha_${index}_old(y) }`,
                    `pub fn gamma_${index}_old() {}`,
                    "",
                ].join("\n");
                writeFileSync(join(dir, name), body);
                expected.set(name, body.replace(REPLACE_REGEX, (_whole, stem: string) => `fn ${stem}_new(`));
                names.push(name);
            }
            return {
                files: 50,
                matchesPerFile: 3,
                pattern: REPLACE_PATTERN,
                replacement: REPLACE_WITH,
            };
        },
        variants: [
            variant({
                id: "A1a",
                side: "A",
                label: "sd one call, 50 paths",
                need: "none",
                clis: ["sd"],
                exec: async (ctx) => {
                    const reply = ctx.meter.runCli("sd", [REPLACE_PATTERN, REPLACE_WITH, ...names.map((name) => join(ctx.dir, name))]);
                    return () => {
                        const verdict = checkFiles(ctx.dir);
                        return {
                            ok: verdict.ok && reply.exitCode === 0,
                            note: `${verdict.note} (exit ${reply.exitCode})`,
                        };
                    };
                },
            }),
            variant({
                id: "A1b",
                side: "A",
                label: "sd per file, 50 spawns",
                need: "none",
                clis: ["sd"],
                exec: async (ctx) => {
                    for (const name of names) {
                        ctx.meter.runCli("sd", [REPLACE_PATTERN, REPLACE_WITH, join(ctx.dir, name)]);
                    }
                    return () => checkFiles(ctx.dir);
                },
            }),
            variant({
                id: "A2x",
                side: "A",
                label: "base file-edit regex in 1 call",
                need: "base",
                possible: false,
                reason: "base file-edit matches exact strings only: no regex or capture groups",
            }),
            variant({
                id: "A2",
                side: "A",
                label: "base fs-search + file-edit exact items",
                need: "base",
                exec: async (ctx) => {
                    const search = await ctx.meter.callTool(ctx.base, "fs-search", {
                        items: [
                            {
                                path: ctx.dir,
                                pattern: REPLACE_PATTERN,
                                contextLines: 0,
                                maxResults: 1000,
                            },
                        ],
                    });
                    const edits: Array<Record<string, unknown>> = [];
                    let currentFile = "";
                    for (const line of search.text.split(/\r?\n/)) {
                        if (/^[A-Za-z]:[\\/]/.test(line)) {
                            currentFile = line.trim();
                            continue;
                        }
                        const hit = /^(\d+):(.*)$/.exec(line);
                        if (!hit || !currentFile) {
                            continue;
                        }
                        for (const match of (hit[2] ?? "").matchAll(REPLACE_REGEX)) {
                            edits.push({
                                file_path: currentFile,
                                old_string: match[0],
                                new_string: `fn ${match[1]}_new(`,
                                expected_replacements: 1,
                            });
                        }
                    }
                    const backend = String(findKey(search.structured, "backend") ?? "");
                    const edit = edits.length > 0 ? await ctx.meter.callTool(ctx.base, "file-edit", { items: edits }) : null;
                    return () => {
                        const verdict = checkFiles(ctx.dir);
                        const failed = edit ? edit.items.filter((item) => !item.ok).length : 0;
                        return {
                            ok: verdict.ok,
                            note: `${verdict.note}; ${edits.length} exact edits from search (${backend}), ${failed} failed items`,
                        };
                    };
                },
            }),
            variant({
                id: "B",
                side: "B",
                label: "file-replace 50 items, 1 call",
                need: "cand",
                exec: async (ctx) => {
                    const reply = await ctx.meter.callTool(ctx.cand, "file-replace", {
                        items: names.map((name) => ({
                            file_path: join(ctx.dir, name),
                            pattern: REPLACE_PATTERN,
                            replacement: REPLACE_WITH,
                            expectedReplacements: 3,
                        })),
                    });
                    return () => {
                        const verdict = checkFiles(ctx.dir);
                        return {
                            ok: verdict.ok && !reply.isError,
                            note: reply.isError ? `${verdict.note}; error ${reply.errorText.slice(0, 160)}` : verdict.note,
                        };
                    };
                },
            }),
        ],
    };
}

interface EncodingCase {
    name: string;
    original: Buffer;
    expected: Buffer;
    pattern: string;
    replacement: string;
}

function classifyBytes(actual: Buffer, entry: EncodingCase): string {
    if (actual.equals(entry.expected)) {
        return "preserved";
    }
    if (actual.equals(entry.original)) {
        return "unchanged";
    }
    return "corrupted";
}

function buildEncodingSafety(): Scenario {
    const cases: EncodingCase[] = [];
    const classifyAll = (dir: string, extra: Map<string, string>): Verdict => {
        const labels = cases.map((entry) => {
            const label = classifyBytes(readBytes(join(dir, entry.name)), entry);
            const detail = extra.get(entry.name);
            return detail ? `${entry.name}=${label}(${detail})` : `${entry.name}=${label}`;
        });
        return {
            ok: labels.every((label) => label.includes("=preserved")),
            note: labels.join(", "),
        };
    };
    return {
        name: "encoding-safety",
        title: "replace inside UTF-16LE BOM, EUC-KR, and CRLF files",
        mutates: true,
        prepare: (dir) => {
            cases.length = 0;
            const utf16Text = "첫째 줄 alpha\n둘째 줄 alpha beta\n";
            const eucText = "한글 문서 alpha\n한글 끝\n";
            const crlfText = "line one beta\r\nline two beta\r\n";
            const anchorText = "first alpha\r\nsecond alpha\r\nthird\r\n";
            cases.push(
                {
                    name: "utf16le-bom.txt",
                    original: encodeUtf16Le(utf16Text),
                    expected: encodeUtf16Le(utf16Text.replace(/alpha/g, "OMEGA")),
                    pattern: "alpha",
                    replacement: "OMEGA",
                },
                {
                    name: "euc-kr.txt",
                    original: encodeEucKr(eucText),
                    expected: encodeEucKr(eucText.replace(/한글/g, "국어")),
                    pattern: "한글",
                    replacement: "국어",
                },
                {
                    name: "crlf.txt",
                    original: Buffer.from(crlfText),
                    expected: Buffer.from(crlfText.replace(/beta/g, "BETA")),
                    pattern: "beta",
                    replacement: "BETA",
                },
                {
                    name: "crlf-anchor.txt",
                    original: Buffer.from(anchorText),
                    expected: Buffer.from(anchorText.replace(/alpha(?=\r\n)/g, "OMEGA")),
                    pattern: "alpha$",
                    replacement: "OMEGA",
                },
            );
            for (const entry of cases) {
                writeFileSync(join(dir, entry.name), entry.original);
            }
            return {
                cases: cases.map((entry) => `${entry.name}: ${entry.pattern} -> ${entry.replacement}`),
                anchorRule: "$ before CRLF counts as end of line (catalog: ^ and $ match at line boundaries)",
            };
        },
        variants: [
            variant({
                id: "A",
                side: "A",
                label: "sd per file",
                need: "none",
                clis: ["sd"],
                expectCorrect: false,
                exec: async (ctx) => {
                    const exits = new Map<string, string>();
                    for (const entry of cases) {
                        const reply = ctx.meter.runCli("sd", [entry.pattern, entry.replacement, join(ctx.dir, entry.name)]);
                        exits.set(entry.name, `exit ${reply.exitCode}`);
                    }
                    return () => classifyAll(ctx.dir, exits);
                },
            }),
            variant({
                id: "B",
                side: "B",
                label: "file-replace 4 items, 1 call",
                need: "cand",
                exec: async (ctx) => {
                    const reply = await ctx.meter.callTool(ctx.cand, "file-replace", {
                        items: cases.map((entry) => ({
                            file_path: join(ctx.dir, entry.name),
                            pattern: entry.pattern,
                            replacement: entry.replacement,
                        })),
                    });
                    return () => {
                        const errors = new Map<string, string>();
                        reply.items.forEach((item, index) => {
                            const entry = cases[index];
                            if (entry && !item.ok) {
                                errors.set(entry.name, `error ${item.error.slice(0, 80)}`);
                            }
                        });
                        if (reply.isError && reply.items.length === 0) {
                            for (const entry of cases) {
                                errors.set(entry.name, `call error ${reply.errorText.slice(0, 80)}`);
                            }
                        }
                        return classifyAll(ctx.dir, errors);
                    };
                },
            }),
        ],
    };
}

function buildAmbiguousCapture(): Scenario {
    const original = "hello world\n";
    const judge = (dir: string, errored: boolean, detail: string): Verdict => {
        const now = readFileSync(join(dir, "word.txt"), "utf8");
        const untouched = now === original;
        const state = untouched ? "untouched" : `rewritten to ${JSON.stringify(now)}`;
        return {
            ok: errored && untouched,
            note: `${errored ? "error reported" : "no error"}, file ${state}; ${detail}`.slice(0, 220),
        };
    };
    return {
        name: "ambiguous-capture",
        title: "pattern (\\w+) with replacement \"$1_x\" must be rejected",
        mutates: true,
        prepare: (dir) => {
            writeFileSync(join(dir, "word.txt"), original);
            return {
                pattern: "(\\w+)",
                replacement: "$1_x",
                trap: "$1_x parses as the group named 1_x: an empty write unless rejected",
            };
        },
        variants: [
            variant({
                id: "A",
                side: "A",
                label: "sd",
                need: "none",
                clis: ["sd"],
                exec: async (ctx) => {
                    const reply = ctx.meter.runCli("sd", ["(\\w+)", "$1_x", join(ctx.dir, "word.txt")]);
                    return () => judge(ctx.dir, reply.exitCode !== 0, `exit ${reply.exitCode}: ${reply.stderr.split("\n")[0] ?? ""}`);
                },
            }),
            variant({
                id: "B",
                side: "B",
                label: "file-replace",
                need: "cand",
                exec: async (ctx) => {
                    const reply = await ctx.meter.callTool(ctx.cand, "file-replace", {
                        items: [
                            {
                                file_path: join(ctx.dir, "word.txt"),
                                pattern: "(\\w+)",
                                replacement: "$1_x",
                            },
                        ],
                    });
                    const errored = reply.isError || reply.items.some((item) => !item.ok);
                    return () => judge(ctx.dir, errored, (reply.errorText || reply.items[0]?.error || "").slice(0, 120));
                },
            }),
        ],
    };
}

// 10. Scenarios: preview and diff ---------------------------------------------
function buildPreview(): Scenario {
    let original = "";
    const oldLine = "timeout = 30";
    const newLine = "timeout = 60";
    const judge = (dir: string, diffText: string, extra: string): Verdict => {
        const unchanged = readFileSync(join(dir, "config.txt"), "utf8") === original;
        const leftover = existsSync(join(dir, "config.txt.bak"));
        const diff = parseUnifiedDiff(diffText);
        const diffOk = diff.removed.join("|") === oldLine && diff.added.join("|") === newLine;
        return {
            ok: unchanged && !leftover && diffOk,
            note: `file ${unchanged ? "unchanged" : "MODIFIED"}${leftover ? ", backup left" : ""}, diff -${diff.removed.length}/+${diff.added.length} ${diffOk ? "exact" : "WRONG"}${extra}`,
        };
    };
    return {
        name: "preview",
        title: "preview one edit as a diff without changing the file",
        mutates: true,
        prepare: (dir) => {
            const lines = Array.from({ length: 40 }, (_unused, index) => (index === 19 ? oldLine : `key_${index} = value ${index}`));
            original = lines.join("\n") + "\n";
            writeFileSync(join(dir, "config.txt"), original);
            return {
                lines: 40,
                edit: `${oldLine} -> ${newLine}`,
            };
        },
        variants: [
            variant({
                id: "A",
                side: "A",
                label: "base copy + edit + git diff + restore",
                need: "base",
                clis: ["git"],
                exec: async (ctx) => {
                    const file = join(ctx.dir, "config.txt");
                    const backup = `${file}.bak`;
                    await ctx.meter.callTool(ctx.base, "path-copy", {
                        items: [
                            {
                                source: file,
                                destination: backup,
                                force: true,
                            },
                        ],
                    });
                    await ctx.meter.callTool(ctx.base, "file-edit", {
                        items: [
                            {
                                file_path: file,
                                old_string: oldLine,
                                new_string: newLine,
                            },
                        ],
                    });
                    const diff = ctx.meter.runCli("git", ["-c", "core.autocrlf=false", "diff", "--no-index", "--no-color", backup, file]);
                    await ctx.meter.callTool(ctx.base, "path-copy", {
                        items: [
                            {
                                source: backup,
                                destination: file,
                                force: true,
                            },
                        ],
                    });
                    await ctx.meter.callTool(ctx.base, "path-remove", { paths: [backup] });
                    return () => judge(ctx.dir, diff.stdout.toString("utf8"), "");
                },
            }),
            variant({
                id: "B",
                side: "B",
                label: "file-edit dryRun:true",
                need: "cand",
                exec: async (ctx) => {
                    const reply = await ctx.meter.callTool(ctx.cand, "file-edit", {
                        items: [
                            {
                                file_path: join(ctx.dir, "config.txt"),
                                old_string: oldLine,
                                new_string: newLine,
                                dryRun: true,
                            },
                        ],
                    });
                    return () => judge(ctx.dir, reply.text, reply.isError ? `; error ${reply.errorText.slice(0, 120)}` : "");
                },
            }),
        ],
    };
}

function buildDiff(): Scenario {
    let insertions = 0;
    let deletions = 0;
    const judge = (label: string, diffText: string, structured: unknown): Verdict => {
        const diff = parseUnifiedDiff(diffText);
        const reportedInsertions = findKey(structured, "insertions");
        const reportedDeletions = findKey(structured, "deletions");
        const countsOk = diff.added.length === insertions && diff.removed.length === deletions;
        const reportedOk = reportedInsertions === undefined || (reportedInsertions === insertions && reportedDeletions === deletions);
        const reported = reportedInsertions === undefined ? "" : `, reported +${String(reportedInsertions)}/-${String(reportedDeletions)}`;
        return {
            ok: countsOk && reportedOk,
            note: `${label} +${diff.added.length}/-${diff.removed.length} in ${diff.hunks} hunks vs +${insertions}/-${deletions}${reported}`,
        };
    };
    return {
        name: "diff",
        title: "unified diff of two 5,000-line files with scattered edits",
        mutates: false,
        prepare: (dir) => {
            const random = seededRandom(7);
            const oldLines = Array.from({ length: 5_000 }, (_unused, index) => `line ${index}: ${Math.floor(random() * 1e9).toString(36)} payload`);
            const newLines: string[] = [];
            let modified = 0;
            let inserted = 0;
            let removed = 0;
            oldLines.forEach((line, index) => {
                const slot = index % 125;
                if (slot === 40) {
                    modified += 1;
                    newLines.push(`${line} changed`);
                }
                else if (slot === 80) {
                    removed += 1;
                }
                else if (slot === 110) {
                    newLines.push(line, `inserted after ${index}`);
                    inserted += 1;
                }
                else {
                    newLines.push(line);
                }
            });
            writeFileSync(join(dir, "old.txt"), oldLines.join("\n") + "\n");
            writeFileSync(join(dir, "new.txt"), newLines.join("\n") + "\n");
            insertions = modified + inserted;
            deletions = modified + removed;
            const gitExe = CLI_PATHS.get("git");
            let numstat = "git missing";
            if (gitExe) {
                const proc = Bun.spawnSync([gitExe, "-c", "core.autocrlf=false", "diff", "--no-index", "--numstat", "old.txt", "new.txt"], {
                    cwd: dir,
                    stdin: "ignore",
                    stdout: "pipe",
                    stderr: "pipe",
                });
                numstat = Buffer.from(proc.stdout ?? new Uint8Array()).toString("utf8").trim();
                const [gitAdded, gitRemoved] = numstat.split(/\s+/).map(Number);
                if (gitAdded !== insertions || gitRemoved !== deletions) {
                    throw new Error(`fixture truth disagrees with git numstat: ${numstat} vs +${insertions}/-${deletions}`);
                }
            }
            return {
                lines: 5_000,
                modified: modified,
                inserted: inserted,
                removed: removed,
                numstat: numstat,
            };
        },
        variants: [
            variant({
                id: "A",
                side: "A",
                label: "git diff --no-index",
                need: "none",
                clis: ["git"],
                exec: async (ctx) => {
                    const reply = ctx.meter.runCli("git", ["-c", "core.autocrlf=false", "diff", "--no-index", "--no-color", join(ctx.dir, "old.txt"), join(ctx.dir, "new.txt")]);
                    return () => judge(`exit ${reply.exitCode}`, reply.stdout.toString("utf8"), undefined);
                },
            }),
            variant({
                id: "B",
                side: "B",
                label: "file-diff",
                need: "cand",
                exec: async (ctx) => {
                    const reply = await ctx.meter.callTool(ctx.cand, "file-diff", {
                        items: [
                            {
                                path: join(ctx.dir, "old.txt"),
                                otherPath: join(ctx.dir, "new.txt"),
                            },
                        ],
                    });
                    return () => {
                        if (reply.isError) {
                            return {
                                ok: false,
                                note: `error ${reply.errorText.slice(0, 160)}`,
                            };
                        }
                        return judge("file-diff", reply.text, reply.structured);
                    };
                },
            }),
        ],
    };
}

// 11. Scenarios: catalog ------------------------------------------------------
const NEW_TOOLS: ReadonlySet<string> = new Set(["path-find", "file-replace", "file-diff"]);

function buildCatalog(): Scenario {
    const facts: Record<string, unknown> = {};
    const inspect = (message: unknown, expectedCount: number, label: string): Verdict => {
        const tools = pick(message, "result", "tools");
        if (!Array.isArray(tools)) {
            return {
                ok: false,
                note: "no tools array",
            };
        }
        const resultBytes = Buffer.byteLength(JSON.stringify(pick(message, "result")));
        const newBytes = tools.filter((tool) => NEW_TOOLS.has(String(pick(tool, "name")))).reduce((sum: number, tool) => sum + Buffer.byteLength(JSON.stringify(tool)), 0);
        facts[`${label}Tools`] = tools.length;
        facts[`${label}ListBytes`] = resultBytes;
        facts[`${label}NewToolBytes`] = newBytes;
        return {
            ok: tools.length === expectedCount,
            note: `${tools.length} tools, ${resultBytes} bytes${newBytes > 0 ? `, 3 new tools ${newBytes} bytes` : ""}`,
        };
    };
    return {
        name: "catalog",
        title: "tools/list size per session and binary size",
        mutates: false,
        prepare: () => {
            facts.baseBinaryBytes = BASE_PATH ? statSync(BASE_PATH).size : null;
            facts.candBinaryBytes = CAND_PATH ? statSync(CAND_PATH).size : null;
            return facts;
        },
        variants: [
            variant({
                id: "A",
                side: "A",
                label: "base tools/list",
                need: "base",
                exec: async (ctx) => {
                    const message = await ctx.meter.listTools(ctx.base);
                    return () => inspect(message, 21, "base");
                },
            }),
            variant({
                id: "B",
                side: "B",
                label: "cand tools/list",
                need: "cand",
                exec: async (ctx) => {
                    const message = await ctx.meter.listTools(ctx.cand);
                    return () => inspect(message, 24, "cand");
                },
            }),
        ],
    };
}

// 12. Runner ------------------------------------------------------------------
function percentile(sorted: number[], fraction: number): number {
    if (sorted.length === 0) {
        return 0;
    }
    const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil(fraction * sorted.length) - 1));
    return sorted[index] ?? 0;
}

function roundMs(value: number): number {
    return Math.round(value * 1000) / 1000;
}

function skipReason(entry: Variant, base: McpClient | null, cand: McpClient | null): string {
    if (!entry.possible) {
        return entry.reason;
    }
    if (entry.need === "base" && !base) {
        return "no --base";
    }
    if (entry.need === "cand" && !cand) {
        return "no --cand";
    }
    const missing = entry.clis.filter((name) => !CLI_PATHS.has(name));
    return missing.length > 0 ? `missing CLI: ${missing.join(", ")}` : "";
}

async function runOnce(scenario: Scenario, entry: Variant, pristine: string, clients: [McpClient | null, McpClient | null], tag: string): Promise<RunSample> {
    let dir = pristine;
    if (scenario.mutates) {
        dir = join(FIXTURES, "work", `${scenario.name}-${entry.id}-${tag}`);
        rmSync(dir, {
            recursive: true,
            force: true,
        });
        cpSync(pristine, dir, { recursive: true });
    }
    const meter = new TaskMeter();
    const ctx: RunContext = {
        base: clients[0],
        cand: clients[1],
        dir: dir,
        meter: meter,
    };
    let verdict: Verdict;
    const started = performance.now();
    let elapsed = 0;
    try {
        const judge = await entry.exec(ctx);
        elapsed = performance.now() - started;
        verdict = judge();
    }
    catch (error) {
        elapsed = performance.now() - started;
        verdict = {
            ok: false,
            note: `harness error: ${String(error instanceof Error ? error.message : error).slice(0, 200)}`,
        };
    }
    if (scenario.mutates) {
        rmSync(dir, {
            recursive: true,
            force: true,
        });
    }
    return {
        ms: elapsed,
        calls: meter.calls,
        outBytes: meter.outBytes,
        inBytes: meter.inBytes,
        ok: verdict.ok,
        note: verdict.note,
    };
}

async function runScenario(scenario: Scenario, base: McpClient | null, cand: McpClient | null): Promise<ScenarioReport> {
    const pristine = join(FIXTURES, scenario.name);
    resetDir(pristine);
    const facts = scenario.prepare(pristine);
    const samples = new Map<string, RunSample[]>();
    const runnable = scenario.variants.filter((entry) => skipReason(entry, base, cand) === "");
    for (const entry of runnable) {
        await runOnce(scenario, entry, pristine, [base, cand], "warmup");
        samples.set(entry.id, []);
    }
    for (let round = 1; round <= ROUNDS; round++) {
        // 짝수 라운드 역순 (캐시 워밍, 부하 편향 상쇄)
        const order = round % 2 === 1 ? runnable : [...runnable].reverse();
        for (const entry of order) {
            samples.get(entry.id)?.push(await runOnce(scenario, entry, pristine, [base, cand], String(round)));
        }
    }
    const variants = scenario.variants.map((entry): VariantReport => {
        const reason = skipReason(entry, base, cand);
        const runs = samples.get(entry.id) ?? [];
        const times = runs.map((sample) => sample.ms).sort((left, right) => left - right);
        const outBytes = runs.map((sample) => sample.outBytes).sort((left, right) => left - right);
        const inBytes = runs.map((sample) => sample.inBytes).sort((left, right) => left - right);
        return {
            id: entry.id,
            side: entry.side,
            label: entry.label,
            status: reason === "" ? "ran" : entry.possible ? "skipped" : "not-possible",
            reason: reason,
            expectCorrect: entry.expectCorrect,
            runs: runs.length,
            correctRuns: runs.filter((sample) => sample.ok).length,
            calls: Math.max(0, ...runs.map((sample) => sample.calls)),
            outBytesP50: percentile(outBytes, 0.5),
            inBytesP50: percentile(inBytes, 0.5),
            minMs: roundMs(times[0] ?? 0),
            p50Ms: roundMs(percentile(times, 0.5)),
            p95Ms: roundMs(percentile(times, 0.95)),
            notes: [...new Set(runs.map((sample) => sample.note))].slice(0, 4),
        };
    });
    return {
        name: scenario.name,
        title: scenario.title,
        facts: facts,
        variants: variants,
    };
}

// 13. Report ------------------------------------------------------------------
function printSummary(reports: ScenarioReport[]): void {
    const rows = [
        "| scenario | variant | calls | out bytes | in bytes | min ms | p50 ms | p95 ms | correct |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
    ];
    for (const report of reports) {
        for (const entry of report.variants) {
            const name = `${entry.side}:${entry.id} ${entry.label}`;
            if (entry.status !== "ran") {
                rows.push(`| ${report.name} | ${name} | - | - | - | - | - | - | ${entry.status}: ${entry.reason} |`);
                continue;
            }
            rows.push(`| ${report.name} | ${name} | ${entry.calls} | ${entry.outBytesP50} | ${entry.inBytesP50} | ${entry.minMs.toFixed(2)} | ${entry.p50Ms.toFixed(2)} | ${entry.p95Ms.toFixed(2)} | ${entry.correctRuns}/${entry.runs} |`);
        }
    }
    console.log(rows.join("\n"));
    console.log("");
    for (const report of reports) {
        console.log(`## ${report.name}: ${report.title}`);
        console.log(`facts ${JSON.stringify(report.facts)}`);
        for (const entry of report.variants.filter((item) => item.status === "ran")) {
            for (const note of entry.notes) {
                console.log(`  ${entry.side}:${entry.id} ${note}`);
            }
        }
    }
}

function probeVersions(): Record<string, string> {
    const versions: Record<string, string> = {};
    for (const name of ["fd", "sd", "git"]) {
        const exe = Bun.which(name);
        if (!exe) {
            continue;
        }
        CLI_PATHS.set(name, exe);
        const proc = Bun.spawnSync([exe, "--version"], {
            stdin: "ignore",
            stdout: "pipe",
            stderr: "pipe",
        });
        const banner = Buffer.from(proc.stdout ?? new Uint8Array()).toString("utf8").split("\n")[0]?.trim() ?? "";
        versions[name] = banner || `present (${exe})`;
    }
    return versions;
}

async function main(): Promise<void> {
    const rootSlash = toSlash(ROOT).toLowerCase();
    const fixtureSlash = toSlash(FIXTURES).toLowerCase();
    if (fixtureSlash === rootSlash || rootSlash.startsWith(fixtureSlash + "/") || fixtureSlash.startsWith(`${rootSlash}/src`)) {
        console.error(`refusing fixtures dir ${FIXTURES}: it would cover repository sources`);
        process.exit(1);
    }
    if (!BASE_PATH && !CAND_PATH) {
        console.error("pass --base <exe> and/or --cand <exe>");
        process.exit(1);
    }
    for (const binary of [BASE_PATH, CAND_PATH].filter(Boolean)) {
        if (!existsSync(binary)) {
            console.error(`binary not found: ${binary}`);
            process.exit(1);
        }
    }
    if (!Number.isInteger(ROUNDS) || ROUNDS < 1) {
        console.error("--rounds must be a positive integer");
        process.exit(1);
    }
    const versions = probeVersions();
    mkdirSync(FIXTURES, { recursive: true });
    const base = BASE_PATH ? new McpClient(resolve(BASE_PATH)) : null;
    const cand = CAND_PATH ? new McpClient(resolve(CAND_PATH)) : null;
    await base?.initialize();
    await cand?.initialize();
    const scenarios = [
        buildFindGlob(),
        buildFindMeta(),
        buildRegexReplace(),
        buildEncodingSafety(),
        buildAmbiguousCapture(),
        buildPreview(),
        buildDiff(),
        buildCatalog(),
    ].filter((scenario) => ONLY_NAMES.length === 0 || ONLY_NAMES.includes(scenario.name));
    const reports: ScenarioReport[] = [];
    for (const scenario of scenarios) {
        process.stdout.write(`running ${scenario.name} ...\n`);
        reports.push(await runScenario(scenario, base, cand));
    }
    await base?.close();
    await cand?.close();
    rmSync(join(FIXTURES, "work"), {
        recursive: true,
        force: true,
    });
    printSummary(reports);
    mkdirSync(dirname(SAVE_PATH), { recursive: true });
    const saved = {
        at: new Date().toISOString(),
        base: BASE_PATH ? resolve(BASE_PATH) : null,
        cand: CAND_PATH ? resolve(CAND_PATH) : null,
        rounds: ROUNDS,
        fixtures: FIXTURES,
        versions: versions,
        scenarios: reports,
    };
    writeFileSync(SAVE_PATH, JSON.stringify(saved, null, 2));
    console.log(`\nsaved ${SAVE_PATH}`);
    const broken = reports.flatMap((report) => report.variants.filter((entry) => entry.status === "ran" && entry.expectCorrect && entry.correctRuns < entry.runs).map((entry) => `${report.name}/${entry.side}:${entry.id}`));
    console.log(broken.length > 0 ? `incorrect: ${broken.join(", ")}` : "every expected-correct variant matched ground truth");
    process.exit(broken.length > 0 ? 2 : 0);
}

await main();
