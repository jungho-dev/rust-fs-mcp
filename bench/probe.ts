// probe.ts
// Dump raw JSON-RPC result shapes for a few tools so benchmark verifiers can be written
// against the real contract instead of a guess.

const root = new URL("..", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1").replace(/\/$/, "");
const binary = process.argv[2] ?? `${root}/target/bench/release/rust-fs-mcp.exe`;

const proc = Bun.spawn([binary], {
  cwd: root,
  env: { ...process.env, RUST_FS_MCP_CONFIG: "none" },
  stdin: "pipe",
  stdout: "pipe",
  stderr: "pipe",
});

const pending = new Map<number, (value: any) => void>();
const decoder = new TextDecoder();
let buffer = "";
let id = 1;

async function readLoop() {
  const reader = (proc.stdout as ReadableStream<Uint8Array>).getReader();
  while (true) {
    const { done, value } = await reader.read();
    if (done) return;
    buffer += decoder.decode(value, { stream: true });
    let index = buffer.indexOf("\n");
    while (index >= 0) {
      const line = buffer.slice(0, index);
      buffer = buffer.slice(index + 1);
      index = buffer.indexOf("\n");
      if (!line) continue;
      const message = JSON.parse(line);
      const resolve = pending.get(message.id);
      if (resolve) {
        pending.delete(message.id);
        resolve(message);
      }
    }
  }
}
readLoop();

function send(method: string, params: any) {
  const current = id++;
  return new Promise<any>((resolve) => {
    pending.set(current, resolve);
    (proc.stdin as any).write(JSON.stringify({ jsonrpc: "2.0", id: current, method, params }) + "\n");
  });
}

await send("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "probe", version: "1" } });
(proc.stdin as any).write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");

const calls: Array<[string, any]> = [
  ["definitely-not-a-tool", {}],
  ["file-read-line-range", { items: [{ path: `${root}/Cargo.toml`, start_line: 1, line_count: 2 }] }],
  ["file-read", { paths: [`${root}/Cargo.toml`] }],
  ["dir-list", { items: [{ path: `${root}/src`, recursive: true }] }],
  ["fs-search", { items: [{ path: `${root}/src`, pattern: "fn handle_file_read", literal: true, maxResults: 5 }] }],
  ["path-stat", { paths: [`${root}/Cargo.toml`] }],
];

for (const [tool, args] of calls) {
  const response = await send("tools/call", { name: tool, arguments: args });
  const text = JSON.stringify(response.result);
  console.log(`\n=== ${tool} (${text.length} chars) ===`);
  console.log(text.slice(0, 1800));
}

(proc.stdin as any).end();
await proc.exited;
