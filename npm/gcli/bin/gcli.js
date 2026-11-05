#!/usr/bin/env node
// Runs the gcli binary from the platform package npm installed alongside this one
// (one of the optionalDependencies, chosen by npm from its "os" and "cpu" fields).
"use strict";

const { spawnSync } = require("child_process");
const path = require("path");

// Windows on ARM runs the x64 build through emulation.
const arch = process.platform === "win32" && process.arch === "arm64" ? "x64" : process.arch;
const pkg = `@castri1/gcli-${process.platform}-${arch}`;
const exe = process.platform === "win32" ? "gcli.exe" : "gcli";

let binary;
try {
  binary = path.join(path.dirname(require.resolve(`${pkg}/package.json`)), "bin", exe);
} catch {
  console.error(`gcli: no prebuilt binary for ${process.platform}-${process.arch} (${pkg} is not installed).`);
  // npm skips an optional dependency it cannot unpack (a damaged cache entry, say) without a word.
  console.error("Reinstall: npm cache verify && npm install -g @castri1/gcli --prefer-online");
  console.error("(not with --omit=optional), or use the installer: https://github.com/castri1/gcli#install");
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`gcli: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
