#!/usr/bin/env node

const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");

const root = path.resolve(__dirname, "..");
const binaryName = process.platform === "win32" ? "portmap.exe" : "portmap";
const sourceBinary = path.join(root, "target", "release", binaryName);
const destDir = path.join(root, "bin", "native");
const destBinary = path.join(destDir, binaryName);
const cargo = process.env.CARGO || "cargo";

const build = spawnSync(cargo, ["build", "--release"], {
  cwd: root,
  stdio: "inherit"
});

if (build.error) {
  if (build.error.code === "ENOENT") {
    console.error("Rust toolchain not found. Install cargo to build the native portmap binary.");
  } else {
    console.error(build.error.message);
  }
  process.exit(1);
}

if ((build.status ?? 1) !== 0) {
  process.exit(build.status ?? 1);
}

fs.mkdirSync(destDir, { recursive: true });
fs.copyFileSync(sourceBinary, destBinary);
fs.chmodSync(destBinary, 0o755);
