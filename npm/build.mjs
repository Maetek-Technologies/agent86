// Assemble the npm packages from the release binaries.
//
//   node npm/build.mjs --version 1.0.6 --bins <dir> --out <dir>
//
// `--bins` holds the extracted release archives, one directory per target whose
// name ends in the target triple (`agent86-v1.0.6-x86_64-unknown-linux-gnu/`), each
// with the `agent86` binary inside. `--out` gets one directory per package: the
// five `@maetek/agent86-<platform>` packages, each holding only its binary, and
// `agent86`, the launcher that depends on them. Publish the platform packages
// first. Without `--version`, the version is read from Cargo.toml.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.dirname(here);

const REPOSITORY = {
  type: "git",
  url: "git+https://github.com/Maetek-Technologies/agent86.git",
};
const HOMEPAGE = "https://github.com/Maetek-Technologies/agent86";

// Release target triple to the npm platform, as `os`, `cpu`, and (Linux) `libc`.
export const TARGETS = {
  "x86_64-unknown-linux-gnu": { platform: "linux-x64", os: "linux", cpu: "x64", libc: "glibc" },
  "aarch64-unknown-linux-gnu": { platform: "linux-arm64", os: "linux", cpu: "arm64", libc: "glibc" },
  "aarch64-apple-darwin": { platform: "darwin-arm64", os: "darwin", cpu: "arm64" },
  "x86_64-apple-darwin": { platform: "darwin-x64", os: "darwin", cpu: "x64" },
  "x86_64-pc-windows-msvc": { platform: "win32-x64", os: "win32", cpu: "x64" },
};

function args() {
  const options = {};
  const argv = process.argv.slice(2);
  for (let i = 0; i < argv.length; i += 2) {
    const name = argv[i].replace(/^--/, "");
    options[name] = argv[i + 1];
  }
  return options;
}

function cargoVersion() {
  const text = fs.readFileSync(path.join(root, "Cargo.toml"), "utf8");
  const match = text.match(/^version\s*=\s*"([^"]+)"/m);
  if (!match) throw new Error("no version in Cargo.toml");
  return match[1];
}

function writeJson(file, value) {
  fs.writeFileSync(file, JSON.stringify(value, null, 2) + "\n");
}

function findBinary(bins, triple) {
  const dir = fs
    .readdirSync(bins)
    .map((name) => path.join(bins, name))
    .find((dir) => fs.statSync(dir).isDirectory() && dir.endsWith(`-${triple}`));
  if (!dir) throw new Error(`no ${triple} directory in ${bins}`);
  const exe = triple.includes("windows") ? "agent86.exe" : "agent86";
  const binary = path.join(dir, exe);
  if (!fs.existsSync(binary)) throw new Error(`no ${exe} in ${dir}`);
  return binary;
}

function main() {
  const options = args();
  const version = (options.version || cargoVersion()).replace(/^v/, "");
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error(`not a release version: ${version}`);
  const bins = options.bins;
  const out = options.out;
  if (!bins || !out) throw new Error("usage: node npm/build.mjs [--version X.Y.Z] --bins DIR --out DIR");
  fs.rmSync(out, { recursive: true, force: true });

  const license = fs.readFileSync(path.join(root, "LICENSE"));
  const optionalDependencies = {};
  for (const [triple, target] of Object.entries(TARGETS)) {
    const name = `@maetek/agent86-${target.platform}`;
    optionalDependencies[name] = version;
    const dir = path.join(out, `agent86-${target.platform}`);
    fs.mkdirSync(path.join(dir, "bin"), { recursive: true });
    const exe = target.os === "win32" ? "agent86.exe" : "agent86";
    fs.copyFileSync(findBinary(bins, triple), path.join(dir, "bin", exe));
    fs.chmodSync(path.join(dir, "bin", exe), 0o755);
    fs.writeFileSync(path.join(dir, "LICENSE"), license);
    fs.writeFileSync(
      path.join(dir, "README.md"),
      `# ${name}\n\nThe agent86 binary for ${target.platform}. Install [agent86](https://www.npmjs.com/package/agent86) instead: \`npm install -g agent86\`.\n`
    );
    const manifest = {
      name,
      version,
      description: `The agent86 binary for ${target.platform}`,
      license: "MIT",
      repository: REPOSITORY,
      homepage: HOMEPAGE,
      os: [target.os],
      cpu: [target.cpu],
      files: [`bin/${exe}`],
      preferUnplugged: true,
    };
    if (target.libc) manifest.libc = [target.libc];
    writeJson(path.join(dir, "package.json"), manifest);
  }

  const dir = path.join(out, "agent86");
  fs.mkdirSync(path.join(dir, "bin"), { recursive: true });
  fs.copyFileSync(path.join(here, "agent86", "bin", "agent86.js"), path.join(dir, "bin", "agent86.js"));
  fs.chmodSync(path.join(dir, "bin", "agent86.js"), 0o755);
  fs.copyFileSync(path.join(here, "README.md"), path.join(dir, "README.md"));
  fs.writeFileSync(path.join(dir, "LICENSE"), license);
  writeJson(path.join(dir, "package.json"), {
    name: "agent86",
    version,
    description:
      "An agentic harness on the command line: connects to remote or local models and lets them use tools and skills.",
    keywords: ["agent", "ai", "llm", "cli", "mcp", "openrouter", "ollama", "anthropic"],
    license: "MIT",
    repository: REPOSITORY,
    homepage: HOMEPAGE,
    bugs: { url: `${HOMEPAGE}/issues` },
    bin: { agent86: "bin/agent86.js" },
    files: ["bin/agent86.js"],
    engines: { node: ">=18" },
    optionalDependencies,
  });
  process.stdout.write(`assembled agent86 ${version} in ${out}\n`);
}

main();
