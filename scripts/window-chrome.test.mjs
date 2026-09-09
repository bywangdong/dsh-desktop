import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { runInNewContext } from "node:vm";

const script = await readFile(new URL("../src-tauri/src/window_chrome.js", import.meta.url), "utf8");

function harness({ href = "http://127.0.0.1:4000/", readyState = "complete", invoke } = {}) {
  const created = [];
  const calls = [];
  const documentListeners = new Map();
  const windowListeners = new Map();
  const addListener = (listeners, type, handler, options) => {
    const entries = listeners.get(type) ?? [];
    entries.push({ handler, options });
    listeners.set(type, entries);
  };
  const element = () => {
    const attributes = new Map();
    return {
      style: {},
      children: [],
      setAttribute: (name, value) => attributes.set(name, value),
      getAttribute: name => attributes.get(name) ?? null,
      hasAttribute: name => attributes.has(name),
      appendChild(child) { this.children.push(child); },
      addEventListener() {},
    };
  };
  const root = element();
  const document = {
    readyState,
    documentElement: root,
    createElement() { const child = element(); created.push(child); return child; },
    querySelector(selector) { return created.find(child => child.hasAttribute(selector.slice(1, -1))); },
    addEventListener: (type, handler, options) => addListener(documentListeners, type, handler, options),
  };
  const window = {
    location: new URL(href),
    __TAURI__: { core: { invoke: async (command, args) => { calls.push([command, args]); return invoke?.(command, args); } } },
    addEventListener: (type, handler, options) => addListener(windowListeners, type, handler, options),
  };
  const context = { document, window };
  const run = () => runInNewContext(script, context);
  const dispatch = (type, event = {}, listeners = documentListeners) => {
    for (const { handler } of listeners.get(type) ?? []) handler(event);
  };
  run();
  return { root, created, calls, documentListeners, windowListeners, run, dispatch };
}

test("window chrome adds no transparent elements above page controls", () => {
  const h = harness();
  assert.equal(h.created.length, 0);
  assert.equal(h.root.getAttribute("data-tauri-drag-region"), "false");
});

test("only the top strip outside native window controls enables Tauri dragging", () => {
  const h = harness();
  for (const [clientX, clientY, expected] of [
    [72, 0, "deep"], [300, 23, "deep"], [71, 10, "false"],
    [300, 24, "false"], [300, 350, "false"], [300, -1, "false"],
  ]) {
    h.dispatch("mousedown", { clientX, clientY, button: 0 });
    assert.equal(h.root.getAttribute("data-tauri-drag-region"), expected);
  }
});

test("drag eligibility is set before Tauri handles the event without cancelling page input", () => {
  const h = harness();
  const handlers = h.documentListeners.get("mousedown");
  assert.equal(handlers?.length, 1);
  assert.equal(handlers[0].options.capture, true);
  const event = {
    clientX: 300, clientY: 12, button: 0,
    preventDefault() { assert.fail("chrome must not cancel control input"); },
    stopImmediatePropagation() { assert.fail("chrome must not stop control input"); },
  };
  h.dispatch("mousedown", event);
  assert.equal(h.root.getAttribute("data-tauri-drag-region"), "deep");
  h.dispatch("mousedown", { ...event, button: 2 });
  assert.equal(h.root.getAttribute("data-tauri-drag-region"), "false");
});

test("mouseup updates the strip for native double-click handling", () => {
  const h = harness();
  assert.equal(h.documentListeners.get("mouseup")?.[0].options.capture, true);
  h.dispatch("mouseup", { clientX: 300, clientY: 10, button: 0, detail: 2 });
  assert.equal(h.root.getAttribute("data-tauri-drag-region"), "deep");
  h.dispatch("mouseup", { clientX: 300, clientY: 200, button: 0, detail: 2 });
  assert.equal(h.root.getAttribute("data-tauri-drag-region"), "false");
});

test("traffic lights follow pointer position and blur without intercepting clicks", async () => {
  const h = harness();
  h.dispatch("pointermove", { clientX: 20, clientY: 12 });
  h.dispatch("pointermove", { clientX: 21, clientY: 12 });
  h.dispatch("pointermove", { clientX: 81, clientY: 12 });
  h.dispatch("pointermove", { clientX: 20, clientY: 12 });
  h.dispatch("blur", {}, h.windowListeners);
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(h.calls.map(([command, args]) => [command, args.visible]), [
    ["set_traffic_lights_visible", true], ["set_traffic_lights_visible", false],
    ["set_traffic_lights_visible", true], ["set_traffic_lights_visible", false],
  ]);
  assert.equal(h.created.length, 0);
});

test("a failed native hover update can retry without adding an input blocker", async () => {
  const h = harness({ invoke: () => Promise.reject(new Error("native update unavailable")) });
  h.dispatch("pointermove", { clientX: 20, clientY: 12 });
  await new Promise(resolve => setImmediate(resolve));
  h.dispatch("pointermove", { clientX: 21, clientY: 12 });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.calls.length, 2);
  assert.equal(h.created.length, 0);
});

test("chrome initializes once after the document becomes ready", () => {
  const h = harness({ href: "tauri://localhost/index.html", readyState: "loading" });
  assert.equal(h.documentListeners.has("mousedown"), false);
  h.dispatch("DOMContentLoaded");
  h.run();
  h.dispatch("DOMContentLoaded");
  assert.equal(h.documentListeners.get("mousedown")?.length, 1);
  assert.equal(h.documentListeners.get("pointermove")?.length, 1);
});

test("untrusted pages receive no host window handlers", () => {
  for (const href of ["https://example.com/", "http://example.com/", "https://127.0.0.1/"]) {
    const h = harness({ href });
    assert.equal(h.created.length, 0);
    assert.equal(h.documentListeners.size, 0);
    assert.equal(h.windowListeners.size, 0);
  }
});
