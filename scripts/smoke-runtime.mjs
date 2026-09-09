#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { access, mkdtemp, readdir, rm } from "node:fs/promises";
import { platform, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
let application;
let download = false;
for (let index = 0; index < args.length; index += 1) {
  if (args[index] === "--app" && args[index + 1]) application = resolve(args[++index]);
  else if (args[index] === "--download") download = true;
  else throw new Error(`Unknown or incomplete argument: ${args[index]}`);
}
if (!application) throw new Error("Usage: npm run runtime:smoke -- --app <packaged executable> [--download]");
await access(application);
const data = await mkdtemp(join(tmpdir(), "dsh-desktop-smoke-"));
const emptyPath = platform() === "win32"
  ? join(process.env.SystemRoot ?? "C:\\Windows", "System32")
  : "/usr/bin:/bin";

function run(mode) {
  const result = spawnSync(application, [], {
    cwd: root,
    env: {
      ...process.env,
      DSH_DESKTOP_RUNTIME_SMOKE: mode,
      DSH_DESKTOP_DATA_DIR: data,
      DSH_DESKTOP_SMOKE_PATH: emptyPath,
    },
    stdio: "inherit",
    timeout: mode === "prepare" ? 20 * 60_000 : 60_000,
    windowsHide: true,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`Packaged runtime ${mode} failed (exit ${result.status})`);
}

try {
  run("inspect");
  if ((await readdir(join(data, "runtime"))).includes("toolchains")) {
    throw new Error("Inspection unexpectedly created toolchains");
  }
  if (download) {
    run("prepare");
    run("cached");
  }
  process.stdout.write(download
    ? "Packaged first-run download and cached launch passed.\n"
    : "Packaged inspection passed without bundled tools or downloads.\n");
} catch (error) {
  process.stderr.write(`Smoke-test data retained for diagnosis: ${data}\n`);
  throw error;
}
await rm(data, { recursive: true, force: true });
