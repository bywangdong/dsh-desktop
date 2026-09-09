import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { runInNewContext } from "node:vm";

const script = await readFile(new URL("../src-tauri/src/new_session.js", import.meta.url), "utf8");

function button(label, { disabled = false, ariaDisabled = false, hidden = false, inert = false } = {}) {
  return {
    label,
    disabled,
    clicks: 0,
    getAttribute: name => name === "aria-disabled" && ariaDisabled ? "true" : null,
    closest: () => inert ? {} : null,
    getClientRects: () => hidden ? [] : [{}],
    click() { this.clicks++; },
  };
}

function run(buttons, { href = "http://127.0.0.1:59980/", embedded = false, activeElement } = {}) {
  const warnings = [];
  const window = { location: new URL(href) };
  window.top = embedded ? {} : window;
  const document = {
    activeElement,
    querySelectorAll: selector => buttons.filter(candidate =>
      selector.includes(`button[aria-label="${candidate.label}"]`)
    ),
  };
  runInNewContext(script, { document, window, console: { warn: message => warnings.push(message) } });
  return warnings;
}

test("new-session command invokes the existing action in either UI language", () => {
  for (const label of ["New session", "新建会话"]) {
    const target = button(label);
    assert.deepEqual(run([target]), []);
    assert.equal(target.clicks, 1);
  }
});

test("expanded sidebar with both brand and new-session buttons starts only once", () => {
  const brand = button("New session");
  const action = button("New session");
  run([brand, action]);
  assert.equal(brand.clicks + action.clicks, 1);
});

test("collapsed sidebar uses its remaining visible action", () => {
  const brand = button("New session", { hidden: true });
  const action = button("New session");
  run([brand, action]);
  assert.equal(brand.clicks, 0);
  assert.equal(action.clicks, 1);
});

test("a focused composer does not block the command or have its draft changed", () => {
  const composer = { value: "Unsent draft" };
  const action = button("New session");
  run([action], { activeElement: composer });
  assert.equal(action.clicks, 1);
  assert.equal(composer.value, "Unsent draft");
});

test("disabled, hidden and inert controls are not activated", () => {
  for (const options of [{ disabled: true }, { ariaDisabled: true }, { hidden: true }, { inert: true }]) {
    const action = button("New session", options);
    assert.equal(run([action]).length, 1);
    assert.equal(action.clicks, 0);
  }
});

test("loading or unsupported pages do not click unrelated controls", () => {
  const action = button("Send message");
  assert.equal(run([action]).length, 1);
  assert.equal(action.clicks, 0);
  assert.equal(run([]).length, 1);
});

test("setup, external and embedded pages receive no new-session actions", () => {
  for (const options of [
    { href: "tauri://localhost/index.html" },
    { href: "http://tauri.localhost/index.html?environment=1" },
    { href: "https://example.com/" },
    { href: "http://127.0.0.1.example.com/" },
    { embedded: true },
  ]) {
    const action = button("New session");
    assert.deepEqual(run([action], options), []);
    assert.equal(action.clicks, 0);
  }
});

test("each subsequent native command can start a session without installing keyboard listeners", () => {
  const action = button("New session");
  run([action]);
  run([action]);
  assert.equal(action.clicks, 2);
});
