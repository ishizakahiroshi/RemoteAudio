import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const source = readFileSync(new URL("../web/app.js", import.meta.url), "utf8").split("// ---------- Boot ----------")[0];
const en = JSON.parse(readFileSync(new URL("../web/lang/en.json", import.meta.url)));
const ja = JSON.parse(readFileSync(new URL("../web/lang/ja.json", import.meta.url)));
const id = "a".repeat(32);
const inventory = { active: null, captures: [{ capture_id: id, bytes: 123, status: "stopped" }] };
class Element {
  constructor(tag) { this.tag = tag; this.children = []; this.attrs = {}; this.events = {}; }
  setAttribute(k, v) { this.attrs[k] = v; }
  addEventListener(k, fn) { this.events[k] = fn; }
  append(...children) { this.children.push(...children); }
  click() { this.clicked = true; }
  remove() { this.removed = true; }
}
function harness(fetchImpl) {
  const body = new Element("body");
  const toast = new Element("toast");
  const calls = [];
  const confirms = [];
  const urls = [];
  const context = vm.createContext({
    Node: Element, Headers, URLSearchParams,
    URL: { createObjectURL(blob) { urls.push(blob); return "blob:synthetic"; }, revokeObjectURL() {} },
    document: { body, getElementById: (id) => id === "toast" ? toast : null,
      createElement: (tag) => new Element(tag), createTextNode: (text) => text },
    localStorage: { getItem() { return ""; }, removeItem() {} },
    location: { hash: "" }, console,
    setTimeout() { return 1; }, clearTimeout() {}, clearInterval() {},
    window: { confirm(text) { confirms.push(text); return true; } },
    fetch: async (path, opts) => { calls.push({ path, opts }); return fetchImpl(path, opts); },
  });
  vm.runInContext(source, context);
  context.strings = en;
  context.inventory = structuredClone(inventory);
  vm.runInContext("state.strings = strings; state.diagnostics = inventory; state.token = 'synthetic-test-token';", context);
  return { context, calls, confirms, urls, body, run: (code) => vm.runInContext(code, context) };
}
function json(data, status = 200) {
  return { ok: status < 400, status, statusText: "synthetic", json: async () => structuredClone(data), text: async () => "conflict" };
}
const deferred = () => { let resolve; const promise = new Promise((r) => { resolve = r; }); return { promise, resolve }; };

test("diagnostics have matching JA/EN keys and clear limitations", () => {
  const keys = Object.keys(en).filter((key) => key.startsWith("diagnostics."));
  assert.deepEqual(keys, Object.keys(ja).filter((key) => key.startsWith("diagnostics.")));
  assert.match(en["diagnostics.privacy"], /No audio is recorded/);
  assert.match(en["diagnostics.privacy"], /whole output.*individual apps.*do not prove/);
  assert.match(en["diagnostics.timing"], /arrives at the host.*network and UI/);
  assert.match(en["diagnostics.confirmPurge"], /\{count\}.*cannot be undone/);
});

test("no capture starts on load; repeated in-flight starts are suppressed", async () => {
  const pending = deferred();
  const h = harness(async (path) => path.endsWith("/start") ? pending.promise : json({ ...inventory, active: id }));
  assert.equal(h.calls.length, 0);
  const first = h.run("setDiagnosticCapture(true)");
  await h.run("setDiagnosticCapture(true)");
  assert.equal(h.calls.length, 1);
  pending.resolve(json({ capture_id: id }));
  await first;
  await h.run("setDiagnosticCapture(true)");
  assert.equal(h.calls.filter((c) => c.path.endsWith("/start")).length, 1);
  assert.equal(h.run("diagnosticsBusy"), false);
});

test("fixed markers only; inactive captures cannot accept markers", async () => {
  const h = harness(async () => json({ ...inventory, active: id }));
  await h.run("addDiagnosticMarker('playback_started')");
  assert.equal(h.calls.length, 0);
  h.run(`state.diagnostics.active = '${id}'`);
  await h.run("addDiagnosticMarker('arbitrary user text')");
  assert.equal(h.calls.length, 0);
  for (const marker of ["playback_started", "stop_pressed", "sound_stopped"]) {
    await h.run(`addDiagnosticMarker('${marker}')`);
  }
  assert.deepEqual(h.calls.filter((c) => c.path.endsWith("/marker")).map((c) => JSON.parse(c.opts.body)), [
    { marker: "playback_started" }, { marker: "stop_pressed" }, { marker: "sound_stopped" },
  ]);
});

test("purge confirms fresh count and cancellation sends no mutation", async () => {
  const h = harness(async (path) => path.endsWith("/purge") ? json({ deleted: 1 }) : json(inventory));
  await h.run("purgeDiagnostics()");
  assert.equal(h.confirms.length, 1);
  assert.match(h.confirms[0], /all 1 saved capture/);
  const purge = h.calls.find((c) => c.path.endsWith("/purge"));
  assert.deepEqual(JSON.parse(purge.opts.body), { confirm_count: 1 });
  assert.equal(purge.opts.method, "POST");
  h.context.window.confirm = () => false;
  await h.run("purgeDiagnostics()");
  assert.equal(h.calls.filter((c) => c.path.endsWith("/purge")).length, 1);
});

test("active capture prevents purge; conflict is not automatically retried", async () => {
  const h = harness(async () => json({ ...inventory, active: id }));
  await h.run("purgeDiagnostics()");
  assert.equal(h.confirms.length, 0);
  assert.equal(h.calls.some((c) => c.path.endsWith("/purge")), false);
  const conflict = harness(async (path) => path.endsWith("/purge") ? json({}, 409) : json(inventory));
  await conflict.run("purgeDiagnostics()");
  assert.equal(conflict.calls.filter((c) => c.path.endsWith("/purge")).length, 1);
  assert.equal(conflict.run("diagnosticsBusy"), false);
});

test("lost mutation reply reads back host state without replaying", async () => {
  const h = harness(async (path) => {
    if (path.endsWith("/start")) throw new Error("connection lost");
    return json({ ...inventory, active: id });
  });
  await h.run("setDiagnosticCapture(true)");
  assert.equal(h.run("state.diagnostics.active"), id);
  assert.equal(h.calls.filter((c) => c.path.endsWith("/start")).length, 1);
});

test("old poll cannot overwrite state following an action", async () => {
  const old = deferred();
  let reads = 0;
  const h = harness(async (path) => {
    if (path === "/api/diagnostics" && ++reads === 1) return old.promise;
    return json({ ...inventory, active: id });
  });
  const read = h.run("refreshDiagnostics()");
  await h.run("setDiagnosticCapture(true)");
  old.resolve(json(inventory));
  await read;
  assert.equal(h.run("state.diagnostics.active"), id);
});

test("failed read clears stale controls; no automatic mutation retry", async () => {
  const h = harness(async () => { throw new Error("offline"); });
  await h.run("setDiagnosticCapture(true)");
  assert.equal(h.run("state.diagnostics"), null);
  assert.equal(h.run("diagnosticsBusy"), false);
  await h.run("setDiagnosticCapture(true)");
  assert.equal(h.calls.length, 2);
});

test("downloads fetch a Bearer-authenticated blob and reject unsafe IDs", async () => {
  const blob = { synthetic: true };
  const h = harness(async (path) => path.endsWith("/download") ? { ok: true, status: 200, blob: async () => blob } : json(inventory));
  await h.run("downloadDiagnosticCapture('../private')");
  assert.equal(h.calls.length, 0);
  await h.run(`downloadDiagnosticCapture('${id}')`);
  const request = h.calls[0];
  assert.equal(request.path, `/api/diagnostics/${id}/download`);
  assert.equal(request.opts.headers.get("authorization"), "Bearer synthetic-test-token");
  assert.equal(request.opts.responseType, undefined);
  assert.equal(h.urls[0], blob);
  const link = h.body.children[0];
  assert.equal(link.attrs.href, "blob:synthetic");
  assert.equal(link.attrs.download, id + ".jsonl");
  assert.equal(link.clicked, true);
  assert.equal(link.removed, true);
});

test("rendered controls disable start, markers and purge for unknown state", () => {
  const h = harness(async () => json(inventory));
  h.run("state.diagnostics = null");
  const panel = h.run("renderDiagnosticsPanel()");
  const buttons = [];
  function walk(node) {
    if (node?.tag === "button") buttons.push(node);
    for (const child of node?.children || []) walk(child);
  }
  walk(panel);
  assert.equal(buttons.filter((b) => !b.disabled).length, 1);
  assert.equal(buttons.find((b) => !b.disabled).children[0], "Refresh");
});
