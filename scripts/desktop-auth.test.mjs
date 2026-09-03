import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { runInNewContext } from "node:vm";

import { desktopEntryUrl } from "../src/desktop-auth.ts";

const initScript = await readFile(
  fileURLToPath(new URL("../src-tauri/src/desktop_auth.js", import.meta.url)),
  "utf8",
);

function runInitScript(href) {
  const current = new URL(href);
  let replacement;
  const location = {
    protocol: current.protocol,
    hostname: current.hostname,
    pathname: current.pathname,
    search: current.search,
    hash: current.hash,
    href: current.href,
    replace(next) {
      replacement = new URL(next).href;
    },
  };
  runInNewContext(initScript, { URL, URLSearchParams, window: { location } });
  return replacement;
}

test("moves the launch token into a fragment for the cross-site handoff", () => {
  assert.equal(
    desktopEntryUrl("http://127.0.0.1:43121/?token=launch-secret"),
    "http://127.0.0.1:43121/#dsh-desktop-token=launch-secret",
  );
  assert.equal(desktopEntryUrl("http://127.0.0.1:43121"), "http://127.0.0.1:43121");
});

test("continues authentication from the loopback page as a same-site navigation", () => {
  assert.equal(
    runInitScript("http://127.0.0.1:43121/#dsh-desktop-token=launch-secret"),
    "http://127.0.0.1:43121/?token=launch-secret",
  );
  assert.equal(runInitScript("tauri://localhost/#dsh-desktop-token=launch-secret"), undefined);
  assert.equal(
    runInitScript("http://example.com/#dsh-desktop-token=launch-secret"),
    undefined,
  );
});
