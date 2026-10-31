// probe-search.ts
// Inspect fs-search result metadata for a fixture with a known match count.

const root = new URL("..", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1").replace(/\/$/, "");
const binary = process.argv[2] ?? `${root}/target/bench/release/rust-fs-mcp.exe`;
const target = `${root}/target/bench-fixtures/hits.txt`;

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
(async () => {
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
})();
function send(method: string, params: any) {
  const current = id++;
  return new Promise<any>((resolve) => {
    pending.set(current, resolve);
    (proc.stdin as any).write(JSON.stringify({ jsonrpc: "2.0", id: current, method, params }) + "\n");
  });
}
await send("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "probe", version: "1" } });
const response = await send("tools/call", {
  name: "fs-search",
  arguments: { items: [{ path: target, pattern: "needle", literal: true, maxResults: 500 }] },
});
const data = response.result.structuredContent.data;
console.log("structured:", JSON.stringify(data.structuredContent));
const text: string = data.content[0].text;
console.log("body chars:", text.length);
console.log("occurrences:", text.split("contains needle").length - 1);
console.log("head:\n" + text.slice(0, 400));
console.log("tail:\n" + text.slice(-300));
(proc.stdin as any).end();
await proc.exited;
