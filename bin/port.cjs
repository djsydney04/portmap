#!/usr/bin/env node

const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");
const { maybePromptForUpgrade } = require("../scripts/update-check.cjs");

const root = path.resolve(__dirname, "..");
const binaryName = process.platform === "win32" ? "cli-cockpit.exe" : "cli-cockpit";
const binaryPath = path.join(root, "bin", "native", binaryName);
const installer = path.join(root, "scripts", "install.js");
const packageJson = require(path.join(root, "package.json"));

async function main() {
  await maybePromptForUpgrade({
    packageName: packageJson.name,
    currentVersion: packageJson.version,
    root,
    cwd: process.cwd()
  });

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
}

main().catch((error) => {
  console.error(error && error.message ? error.message : String(error));
  process.exit(1);
});
