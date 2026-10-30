#!/usr/bin/env node
// The `agent86` command for npm installs: find the binary that npm installed
// for this platform (an optional dependency) and run it, passing the arguments,
// the terminal, and the exit status through.
"use strict";

const { spawn } = require("child_process");
const fs = require("fs");

const RELEASES = "https://github.com/Maetek-Technologies/agent86/releases";

// `process.platform-process.arch` to the package holding that binary.
const PACKAGES = {
  "linux-x64": "@maetek/agent86-linux-x64",
  "linux-arm64": "@maetek/agent86-linux-arm64",
  "darwin-arm64": "@maetek/agent86-darwin-arm64",
  "darwin-x64": "@maetek/agent86-darwin-x64",
  "win32-x64": "@maetek/agent86-win32-x64",
};

function fail(message) {
  process.stderr.write(`agent86: ${message}\n`);
  process.exit(1);
}

const key = `${process.platform}-${process.arch}`;
const pkg = PACKAGES[key];
if (!pkg) {
  fail(
    `there is no prebuilt binary for ${key}. Download one from ${RELEASES}, ` +
      "or build it with: cargo install --git https://github.com/Maetek-Technologies/agent86"
  );
}

// The Linux binaries need glibc; on musl (Alpine) the report has no glibc version.
if (process.platform === "linux" && process.report) {
  const header = process.report.getReport().header;
  if (header && !header.glibcVersionRuntime) {
    fail(
      "the Linux binaries need glibc 2.35 or newer, and this system has none (musl?). " +
        `Build agent86 with cargo instead: see ${RELEASES}`
    );
  }
}

const exe = process.platform === "win32" ? "agent86.exe" : "agent86";
let binary;
try {
  binary = require.resolve(`${pkg}/bin/${exe}`);
} catch (_) {
  fail(
    `${pkg}, which holds the binary, is not installed. Reinstall with ` +
      "npm install -g agent86, without --omit=optional or --no-optional."
  );
}

// Some extractors drop the executable bit; put it back if so.
if (process.platform !== "win32") {
  try {
    fs.accessSync(binary, fs.constants.X_OK);
  } catch (_) {
    try {
      fs.chmodSync(binary, 0o755);
    } catch (_) {
      // Spawning will say what is wrong.
    }
  }
}

// `/update` reads this to know agent86 came from npm.
const env = Object.assign({}, process.env, { AGENT86_INSTALLER: "npm" });
const child = spawn(binary, process.argv.slice(2), { stdio: "inherit", env });

// Ctrl+C and Ctrl+Break reach the child from the terminal directly, and agent86
// uses them to cancel a turn, so the launcher must not die of them first. Signals
// sent to the launcher alone are passed on.
process.on("SIGINT", () => {});
process.on("SIGBREAK", () => {});
for (const signal of ["SIGTERM", "SIGHUP"]) {
  process.on(signal, () => {
    try {
      child.kill(signal);
    } catch (_) {
      // Already gone.
    }
  });
}

child.on("error", (error) => fail(`could not run ${binary}: ${error.message}`));
child.on("exit", (code, signal) => {
  if (signal) {
    // End the same way, so the shell sees the signal and not a plain status.
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code === null ? 1 : code);
});
