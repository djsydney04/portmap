#!/usr/bin/env node

const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");

const root = path.resolve(__dirname, "..");
const binaryName = process.platform === "win32" ? "portmap.exe" : "portmap";
const binaryPath = path.join(root, "bin", "native", binaryName);
const installer = path.join(root, "scripts", "install.js");

if (!fs.existsSync(binaryPath)) {
  const install = spawnSync(process.execPath, [installer, "--build-only"], {
    cwd: root,
    stdio: "inherit"
  });

  if ((install.status ?? 1) !== 0) {
    process.exit(install.status ?? 1);
  }
}

const result = spawnSync(binaryPath, process.argv.slice(2), {
  cwd: process.cwd(),
  stdio: "inherit"
});

if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}

process.exit(result.status ?? 0);
