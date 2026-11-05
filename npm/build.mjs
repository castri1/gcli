// Assembles the npm packages for a release from its GitHub release archives:
// @castri1/gcli (a small launcher) plus one package per platform holding the binary.
// npm installs only the platform package matching the machine ("os" and "cpu").
//
//   node npm/build.mjs v0.1.0 <dir with the release archives> <out dir>
//
// Then publish every out/gcli-* package, and out/gcli last.
import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [tag, archives, out] = process.argv.slice(2);
if (!tag || !archives || !out) {
  console.error("usage: node npm/build.mjs <tag> <archives dir> <out dir>");
  process.exit(1);
}
const version = tag.replace(/^v/, "");
const root = join(dirname(fileURLToPath(import.meta.url)), "..");

const TARGETS = [
  { target: "aarch64-apple-darwin", os: "darwin", cpu: ["arm64"] },
  { target: "x86_64-apple-darwin", os: "darwin", cpu: ["x64"] },
  { target: "x86_64-unknown-linux-musl", os: "linux", cpu: ["x64"] },
  { target: "aarch64-unknown-linux-musl", os: "linux", cpu: ["arm64"] },
  // Windows on ARM runs the x64 build through emulation (see bin/gcli.js).
  { target: "x86_64-pc-windows-msvc", os: "win32", cpu: ["x64", "arm64"] },
];

const common = {
  version,
  license: "MIT",
  repository: { type: "git", url: "git+https://github.com/castri1/gcli.git" },
  homepage: "https://github.com/castri1/gcli#readme",
};

const writeJson = (file, data) => writeFileSync(file, JSON.stringify(data, null, 2) + "\n");

rmSync(out, { recursive: true, force: true });
const optionalDependencies = {};

for (const { target, os, cpu } of TARGETS) {
  const name = `@castri1/gcli-${os}-${cpu[0]}`;
  const dir = join(out, `gcli-${os}-${cpu[0]}`);
  const archive = `gcli-${tag}-${target}`;
  const windows = os === "win32";
  mkdirSync(join(dir, "bin"), { recursive: true });

  const tmp = join(out, ".extract", target);
  mkdirSync(tmp, { recursive: true });
  if (windows) {
    execFileSync("unzip", ["-q", join(archives, `${archive}.zip`), "-d", tmp]);
  } else {
    execFileSync("tar", ["xzf", join(archives, `${archive}.tar.gz`), "-C", tmp]);
  }
  const exe = windows ? "gcli.exe" : "gcli";
  copyFileSync(join(tmp, archive, exe), join(dir, "bin", exe));
  chmodSync(join(dir, "bin", exe), 0o755);

  writeJson(join(dir, "package.json"), {
    name,
    ...common,
    description: `gcli binary for ${os} ${cpu.join("/")}. Install @castri1/gcli instead.`,
    os: [os],
    cpu,
    files: ["bin"],
    preferUnplugged: true,
  });
  optionalDependencies[name] = version;
}
rmSync(join(out, ".extract"), { recursive: true, force: true });

const main = join(out, "gcli");
mkdirSync(join(main, "bin"), { recursive: true });
copyFileSync(join(root, "npm", "gcli", "bin", "gcli.js"), join(main, "bin", "gcli.js"));
chmodSync(join(main, "bin", "gcli.js"), 0o755);
copyFileSync(join(root, "README.md"), join(main, "README.md"));
copyFileSync(join(root, "LICENSE"), join(main, "LICENSE"));
writeJson(join(main, "package.json"), {
  name: "@castri1/gcli",
  ...common,
  description: "Terminal UI and CLI to explore and operate Google Cloud",
  keywords: ["google-cloud", "gcp", "cloud-run", "billing", "cli", "tui", "mcp"],
  bin: { gcli: "bin/gcli.js" },
  files: ["bin", "README.md", "LICENSE"],
  engines: { node: ">=14" },
  optionalDependencies,
});

console.log(`npm packages for ${version} in ${out}`);
