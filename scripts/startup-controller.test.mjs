import assert from "node:assert/strict";
import test from "node:test";
import { StartupController, localAction } from "../src/startup-controller.ts";

const tool = { path: "/user/bin/dsh", binDir: "/user/bin", version: "1.2.3" };
const available = { phase: "ready", ready: true, source: "local", tools: { node: tool, npm: tool, pnpm: tool, dsh: tool, detail: null }, detail: null, saved: false };
const missing = { ...available, phase: "idle", ready: false, source: null, tools: { node: null, npm: null, pnpm: null, dsh: null, detail: null } };

function harness(handler, settings = false) {
  const calls = [], states = [], opened = [];
  const controller = new StartupController({ invoke: async (name, args) => { calls.push([name, args]); return handler(name, args); }, open: url => opened.push(url) }, state => states.push(state), settings);
  return { controller, calls, states, opened };
}

test("an existing runtime skips setup without downloading", async () => {
  const h = harness(name => name === "inspect_runtime" ? available : { url: "http://127.0.0.1:4000/?token=example" });
  await h.controller.inspect("current", null, true);
  assert.deepEqual(h.calls.map(([name]) => name), ["inspect_runtime", "bootstrap"]);
  assert.deepEqual(h.opened, ["http://127.0.0.1:4000/?token=example"]);
  assert.equal(h.controller.state.screen, "launching");
});

test("missing tools do not start a download until the user asks", async () => {
  const h = harness(() => missing);
  await h.controller.inspect("current", null, true);
  assert.equal(h.controller.state.screen, "welcome");
  assert.equal(h.controller.state.busy, false);
  assert.deepEqual(h.calls.map(([name]) => name), ["inspect_runtime"]);
});

test("manual validation failures remain editable and clear stale candidates", async () => {
  const h = harness((_name, args) => { if (args.path === "bad") throw new Error("invalid path"); return available; });
  await h.controller.inspect("local", tool.path);
  assert.equal(localAction(h.controller.state), "start");
  await h.controller.inspect("local", "bad");
  assert.equal(h.controller.state.screen, "local");
  assert.equal(h.controller.state.path, "bad");
  assert.equal(h.controller.state.snapshot, null);
  assert.equal(localAction(h.controller.state), "detect");
});

test("an existing DSH with missing plugin tooling offers only the missing preparation", async () => {
  const h = harness(() => ({ ...available, ready: false, tools: { ...available.tools, pnpm: null } }));
  await h.controller.inspect("local", tool.path);
  assert.equal(localAction(h.controller.state), "prepare");
});

test("cancellation waits for the actual backend job and does not claim success", async () => {
  let rejectPreparation;
  const h = harness(name => {
    if (name === "prepare_runtime") return new Promise((_resolve, reject) => { rejectPreparation = reject; });
    if (name === "cancel_setup") return undefined;
    return missing;
  });
  const preparation = h.controller.prepare("managed", null);
  assert.equal(h.controller.state.screen, "preparing");
  await h.controller.cancel();
  assert.equal(h.controller.state.cancelling, true);
  assert.equal(h.controller.state.busy, true);
  rejectPreparation(new Error("cancelled"));
  await preparation;
  assert.equal(h.controller.state.screen, "welcome");
  assert.equal(h.controller.state.busy, false);
  assert.equal(h.states.some(state => state.screen === "ready"), false);
});

test("cancelling local preparation returns to the same local selection", async () => {
  let rejectPreparation;
  const h = harness(name => {
    if (name === "prepare_runtime") return new Promise((_resolve, reject) => { rejectPreparation = reject; });
    return undefined;
  });
  const preparation = h.controller.prepare("local", tool.path);
  await h.controller.cancel();
  rejectPreparation(new Error("cancelled"));
  await preparation;
  assert.equal(h.controller.state.screen, "local");
  assert.equal(h.controller.state.path, tool.path);
  assert.equal(h.controller.state.mode, "local");
});

test("failed installation retries the same explicit selection", async () => {
  let attempts = 0;
  const h = harness(name => {
    if (name === "prepare_runtime" && ++attempts === 1) throw new Error("network unavailable");
    return available;
  });
  await h.controller.prepare("local", tool.path);
  assert.equal(h.controller.state.screen, "error");
  await h.controller.retry();
  assert.equal(h.controller.state.screen, "ready");
  assert.deepEqual(h.calls.filter(([name]) => name === "prepare_runtime").map(([, args]) => args), [{ mode: "local", path: tool.path }, { mode: "local", path: tool.path }]);
});

test("settings require an explicit apply and never open a second runtime automatically", async () => {
  const h = harness(() => available, true);
  await h.controller.inspect("current", null, true);
  assert.deepEqual(h.calls.map(([name]) => name), ["inspect_runtime"]);
  await h.controller.start();
  assert.deepEqual(h.calls.map(([name]) => name), ["inspect_runtime", "apply_runtime"]);
  assert.deepEqual(h.opened, []);
});

test("a broken saved selection does not start or silently switch providers", async () => {
  const h = harness(() => ({ ...missing, saved: true, source: "local", detail: "the selected installation is unavailable" }));
  await h.controller.inspect("current", null, true);
  assert.equal(h.controller.state.screen, "welcome");
  assert.match(h.controller.state.detail, /unavailable/);
  assert.equal(h.calls.length, 1);
});
