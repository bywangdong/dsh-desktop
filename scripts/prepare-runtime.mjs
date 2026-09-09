#!/usr/bin/env node

import { readFile, access } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const config = JSON.parse(await readFile(join(root, "src-tauri/tauri.conf.json"), "utf8"));
const resources = config.bundle.resources;
const names = Object.keys(resources);
if (names.length !== 1 || resources["resources/node-launcher.mjs"] !== "node-launcher.mjs") {
  throw new Error("The lightweight bundle must contain only the Node entry launcher, not Node, npm, pnpm, or DSH.");
}
await access(join(root, "src-tauri/resources/node-launcher.mjs"));
process.stdout.write("Lightweight resources verified. Runtime components are prepared on first use.\n");
