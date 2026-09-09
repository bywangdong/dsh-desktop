import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const config = JSON.parse(await readFile(new URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8"));
test("production resources contain no bundled runtime", () => {
  assert.deepEqual(config.bundle.resources, { "resources/node-launcher.mjs": "node-launcher.mjs" });
});
test("main and runtime settings preserve the chromeless native shell", () => {
  for (const name of ["main", "environment"]) {
    const window = config.app.windows.find(window => window.label === name);
    assert.equal(window.hiddenTitle, true);
    assert.equal(window.titleBarStyle, "Overlay");
  }
});
test("the first-run page does not contain simulated native controls", async () => {
  const html = await readFile(new URL("../index.html", import.meta.url), "utf8");
  assert.doesNotMatch(html, /window-bar|traffic-light|selected-reference|模拟本机/);
  assert.match(html, /先准备好运行所需的组件/);
  assert.match(html, /使用已有安装/);
});

test("desktop typography and controls do not scale with window width", async () => {
  const css = await readFile(new URL("../src/startup.css", import.meta.url), "utf8");
  assert.doesNotMatch(css, /font-size:\s*[^;{}]*v[wh]/u);
  assert.match(css, /h1\s*\{\s*font-size: 28px/u);
  assert.match(css, /\.primary-button\s*\{[^}]*font-size: 13px/u);
  assert.match(css, /\.app-icon\s*\{[^}]*width: 56px/u);
});

test("runtime settings open as a compact utility window", () => {
  const window = config.app.windows.find(window => window.label === "environment");
  assert.equal(window.width, 600);
  assert.equal(window.height, 480);
  assert.equal(window.resizable, false);
  assert.equal(window.maximizable, false);
  assert.equal(window.skipTaskbar, true);
});

test("startup actions stay inside the bounded content panel", async () => {
  const css = await readFile(new URL("../src/startup.css", import.meta.url), "utf8");
  const html = await readFile(new URL("../index.html", import.meta.url), "utf8");
  assert.match(css, /\.setup-panel\s*\{[^}]*max-width: 480px/u);
  assert.doesNotMatch(css, /\.actions\s*\{[^}]*(?:position: fixed|border-top|min-height: 72px)/u);
  assert.match(html, /class="setup-panel">[\s\S]*<footer class="actions">[\s\S]*<\/footer>\s*<\/div>/u);
});
