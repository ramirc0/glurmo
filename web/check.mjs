// Loads the web view in headless Chromium through a TCP proxy, prints the console, saves a screenshot and
// fails on any error. With --pull, it cuts the proxy for a while and checks that the stream resumed with
// deltas and no fresh snapshot.
//
// Usage: node web/check.mjs SERVER OUT.png [--size 1920x1080] [--query '?layout=tv'] [--settle 8] [--pull 3]
//        [--eval EXPRESSION], which prints the expression's value after the page settles.

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createConnection, createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

const [server, out, ...rest] = process.argv.slice(2);
if (!server || !out) {
  console.error("usage: node web/check.mjs SERVER OUT.png [--size WxH] [--query Q] [--settle S] [--pull S]");
  process.exit(2);
}
const option = (name, fallback) => {
  const at = rest.indexOf(`--${name}`);
  return at >= 0 ? rest[at + 1] : fallback;
};
const [width, height] = option("size", "1920x1080").split("x").map(Number);
const query = option("query", "");
const settle = Number(option("settle", "8"));
const pull = option("pull", null);
const expression = option("eval", null);
const chrome = process.env.CHROME ?? `${process.env.HOME}/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome`;
const sleep = (seconds) => new Promise((done) => setTimeout(done, seconds * 1000));

const upstream = new URL(server);
const sockets = new Set();
let open = true;
const proxy = createServer((client) => {
  if (!open) return client.destroy();
  const backend = createConnection(Number(upstream.port || 80), upstream.hostname);
  for (const socket of [client, backend]) {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => {});
  }
  client.pipe(backend).pipe(client);
});
await new Promise((done) => proxy.listen(0, "127.0.0.1", done));
const url = `http://127.0.0.1:${proxy.address().port}/${query}`;

const profile = mkdtempSync(join(process.env.SCRATCH ?? tmpdir(), "chrome-"));
const browser = spawn(chrome, [
  "--headless=new",
  "--disable-gpu",
  "--no-sandbox",
  "--hide-scrollbars",
  "--remote-debugging-port=0",
  `--user-data-dir=${profile}`,
  "about:blank",
]);
const endpoint = await new Promise((done, fail) => {
  let text = "";
  browser.stderr.on("data", (chunk) => {
    text += chunk;
    const found = text.match(/DevTools listening on (ws:\/\/\S+)/);
    if (found) done(found[1]);
  });
  browser.on("exit", () => fail(new Error(`chrome exited: ${text}`)));
});

const socket = new WebSocket(endpoint);
await new Promise((done) => socket.addEventListener("open", done));
let nextId = 0;
const pending = new Map();
const listeners = [];
socket.addEventListener("message", ({ data }) => {
  const message = JSON.parse(data);
  if (message.id !== undefined) {
    const { resolve, reject } = pending.get(message.id);
    pending.delete(message.id);
    message.error ? reject(new Error(JSON.stringify(message.error))) : resolve(message.result);
  } else {
    for (const listen of listeners) listen(message);
  }
});
const send = (method, params = {}, sessionId) =>
  new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    socket.send(JSON.stringify({ id, method, params, sessionId }));
  });

const { targetId } = await send("Target.createTarget", { url: "about:blank" });
const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
const page = (method, params) => send(method, params, sessionId);

const lines = [];
let failed = false;
const describe = (arg) => arg.value ?? arg.description ?? arg.unserializableValue ?? arg.type;
listeners.push(({ method, params }) => {
  if (method === "Runtime.consoleAPICalled") {
    const text = params.args.map(describe).join(" ");
    lines.push(`console.${params.type}: ${text}`);
    if (params.type === "error") failed = true;
  } else if (method === "Runtime.exceptionThrown") {
    const details = params.exceptionDetails;
    lines.push(`exception: ${details.exception?.description ?? details.text}`);
    failed = true;
  } else if (method === "Log.entryAdded" && params.entry.level === "error") {
    lines.push(`log.error: ${params.entry.text} ${params.entry.url ?? ""}`);
    if (!(pull && params.entry.text.includes("ERR_"))) failed = true;
  }
});
await page("Runtime.enable");
await page("Log.enable");
await page("Page.enable");
await page("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
await page("Page.navigate", { url });
await sleep(settle);

const stats = async () => {
  const { result } = await page("Runtime.evaluate", { expression: "JSON.stringify(window.rtviz ?? null)", returnByValue: true });
  return JSON.parse(result.value);
};
if (pull) {
  const before = await stats();
  open = false;
  for (const socket of sockets) socket.destroy();
  await sleep(Number(pull));
  const pulled = out.replace(/\.png$/, "-pulled.png");
  const shot = await page("Page.captureScreenshot", { format: "png" });
  writeFileSync(pulled, Buffer.from(shot.data, "base64"));
  lines.push(`screenshot while pulled: ${pulled}`);
  open = true;
  await sleep(settle);
  const after = await stats();
  lines.push(`stream before the pull: ${JSON.stringify(before)}`);
  lines.push(`stream after the pull:  ${JSON.stringify(after)}`);
  if (!before || !after || after.snapshots !== before.snapshots || after.deltas <= before.deltas || after.reconnects <= before.reconnects) {
    lines.push("the stream did not resume with deltas alone");
    failed = true;
  }
}

if (expression) {
  const { result } = await page("Runtime.evaluate", { expression: `JSON.stringify(${expression})`, returnByValue: true, awaitPromise: true });
  lines.push(`${expression} = ${result.value}`);
}

const { data } = await page("Page.captureScreenshot", { format: "png" });
writeFileSync(out, Buffer.from(data, "base64"));
lines.push(`screenshot: ${out}`);
console.log(lines.join("\n"));

socket.close();
browser.kill();
proxy.close();
for (const socket of sockets) socket.destroy();
await sleep(0.5);
rmSync(profile, { recursive: true, force: true });
process.exit(failed ? 1 : 0);
