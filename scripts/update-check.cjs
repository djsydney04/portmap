const fs = require("fs");
const os = require("os");
const path = require("path");
const readline = require("node:readline");
const { spawnSync } = require("child_process");

const CHECK_INTERVAL_MS = 12 * 60 * 60 * 1000;
const PROMPT_INTERVAL_MS = 24 * 60 * 60 * 1000;
const UPDATE_ENV_DISABLE = "CLI_COCKPIT_DISABLE_UPDATE_CHECK";
const LEGACY_UPDATE_ENV_DISABLE = "PORTLEDGER_DISABLE_UPDATE_CHECK";

async function maybePromptForUpgrade({
  packageName,
  currentVersion,
  root,
  cwd = process.cwd(),
  now = Date.now(),
  stdin = process.stdin,
  stderr = process.stderr,
  env = process.env,
  cachePath = defaultCachePath(packageName),
  execNpmView = npmViewLatestVersion,
  askYesNo = promptYesNo,
  runInstall = runInstallPlan
}) {
  if (!shouldRunUpdateCheck({ stdin, stderr, env, root })) {
    return { checked: false, upgraded: false };
  }

  const cache = readCache(cachePath);
  let nextCache = { ...cache };
  let latestVersion = cache.latestVersion;

  if (shouldRefreshCache(cache, now)) {
    const refreshed = execNpmView(packageName);
    if (refreshed) {
      latestVersion = refreshed;
      nextCache.latestVersion = refreshed;
    }
    nextCache.checkedAt = now;
    writeCache(cachePath, nextCache);
  }

  if (!latestVersion || compareVersions(latestVersion, currentVersion) <= 0) {
    return { checked: true, upgraded: false, latestVersion };
  }

  if (!shouldPrompt(cache, latestVersion, now)) {
    return { checked: true, upgraded: false, latestVersion };
  }

  const installPlan = resolveInstallPlan({ packageName, root, cwd });
  const question = formatUpgradeQuestion({
    packageName,
    latestVersion,
    currentVersion,
    installPlan
  });
  const accepted = await askYesNo({
    question,
    stdin,
    stderr
  });

  nextCache = {
    ...nextCache,
    latestVersion,
    promptedVersion: latestVersion,
    promptedAt: now
  };
  writeCache(cachePath, nextCache);

  if (!accepted) {
    return { checked: true, upgraded: false, latestVersion, declined: true };
  }

  const result = runInstall(installPlan);
  return {
    checked: true,
    upgraded: result.ok,
    latestVersion,
    installPlan
  };
}

function shouldRunUpdateCheck({ stdin, stderr, env, root }) {
  if (!stdin || !stdin.isTTY || !stderr || !stderr.isTTY) {
    return false;
  }
  if (env.CI) {
    return false;
  }
  if (env[UPDATE_ENV_DISABLE] === "1" || env[LEGACY_UPDATE_ENV_DISABLE] === "1") {
    return false;
  }
  if (root && fs.existsSync(path.join(root, ".git"))) {
    return false;
  }
  return true;
}

function shouldRefreshCache(cache, now) {
  if (!cache.checkedAt) {
    return true;
  }
  return now - cache.checkedAt >= CHECK_INTERVAL_MS;
}

function shouldPrompt(cache, latestVersion, now) {
  if (!cache.promptedVersion || cache.promptedVersion !== latestVersion) {
    return true;
  }
  if (!cache.promptedAt) {
    return true;
  }
  return now - cache.promptedAt >= PROMPT_INTERVAL_MS;
}

function npmViewLatestVersion(packageName) {
  const command = process.platform === "win32" ? "npm.cmd" : "npm";
  const result = spawnSync(command, ["view", packageName, "version", "--json"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 1500
  });

  if (result.error || (result.status ?? 1) !== 0 || !result.stdout) {
    return null;
  }

  return parseLatestVersion(result.stdout);
}

async function promptYesNo({ question, stdin, stderr }) {
  const rl = readline.createInterface({
    input: stdin,
    output: stderr
  });

  try {
    const answer = await new Promise((resolve) => {
      rl.question(`${question} [Y/n] `, resolve);
    });
    const normalized = String(answer).trim().toLowerCase();
    return normalized === "" || normalized === "y" || normalized === "yes";
  } finally {
    rl.close();
  }
}

function resolveInstallPlan({ packageName, root, cwd }) {
  const versionSpec = `${packageName}@latest`;
  const localProjectRoot = findLocalProjectRoot({ packageName, root, cwd });
  if (localProjectRoot) {
    return {
      command: npmCommand(),
      args: ["install", versionSpec],
      cwd: localProjectRoot,
      label: "project"
    };
  }

  return {
    command: npmCommand(),
    args: ["install", "-g", versionSpec],
    cwd,
    label: "global"
  };
}

function findLocalProjectRoot({ packageName, root, cwd }) {
  const expected = safeRealpath(root);
  let current = safeRealpath(cwd);

  while (current) {
    const candidate = path.join(current, "node_modules", packageName);
    if (fs.existsSync(candidate) && safeRealpath(candidate) === expected) {
      return current;
    }
    const parent = path.dirname(current);
    if (parent === current) {
      break;
    }
    current = parent;
  }

  return null;
}

function safeRealpath(target) {
  try {
    return fs.realpathSync(target);
  } catch (_) {
    return path.resolve(target);
  }
}

function formatUpgradeQuestion({
  packageName,
  latestVersion,
  currentVersion,
  installPlan
}) {
  const scope = installPlan.label === "project" ? "this project" : "the global install";
  return `${packageName} ${latestVersion} is available (current ${currentVersion}). Upgrade ${scope} now?`;
}

function runInstallPlan(installPlan) {
  const result = spawnSync(installPlan.command, installPlan.args, {
    cwd: installPlan.cwd,
    stdio: "inherit"
  });

  if (result.error) {
    return { ok: false, error: result.error };
  }

  return { ok: (result.status ?? 1) === 0 };
}

function npmCommand() {
  return process.platform === "win32" ? "npm.cmd" : "npm";
}

function parseLatestVersion(stdout) {
  const trimmed = String(stdout).trim();
  if (!trimmed) {
    return null;
  }

  try {
    const parsed = JSON.parse(trimmed);
    if (typeof parsed === "string") {
      return normalizeVersion(parsed);
    }
    if (Array.isArray(parsed) && typeof parsed[0] === "string") {
      return normalizeVersion(parsed[parsed.length - 1]);
    }
  } catch (_) {
    return normalizeVersion(trimmed);
  }

  return null;
}

function compareVersions(left, right) {
  const a = versionParts(left);
  const b = versionParts(right);
  const max = Math.max(a.length, b.length);
  for (let index = 0; index < max; index += 1) {
    const lhs = a[index] ?? 0;
    const rhs = b[index] ?? 0;
    if (lhs > rhs) {
      return 1;
    }
    if (lhs < rhs) {
      return -1;
    }
  }
  return 0;
}

function versionParts(version) {
  return normalizeVersion(version)
    .split(".")
    .map((part) => Number.parseInt(part, 10))
    .map((part) => (Number.isFinite(part) ? part : 0));
}

function normalizeVersion(version) {
  return String(version)
    .trim()
    .replace(/^v/i, "")
    .split("-")[0];
}

function defaultCachePath(packageName) {
  return path.join(defaultCacheDir(), packageName, "update-check.json");
}

function defaultCacheDir() {
  if (process.env.XDG_CACHE_HOME) {
    return process.env.XDG_CACHE_HOME;
  }
  if (process.platform === "darwin") {
    return path.join(os.homedir(), "Library", "Caches");
  }
  return path.join(os.homedir(), ".cache");
}

function readCache(cachePath) {
  try {
    const raw = fs.readFileSync(cachePath, "utf8");
    const parsed = JSON.parse(raw);
    return typeof parsed === "object" && parsed ? parsed : {};
  } catch (_) {
    return {};
  }
}

function writeCache(cachePath, value) {
  try {
    fs.mkdirSync(path.dirname(cachePath), { recursive: true });
    fs.writeFileSync(cachePath, JSON.stringify(value, null, 2));
  } catch (_) {
    // Ignore cache write failures. The update prompt is best-effort.
  }
}

module.exports = {
  CHECK_INTERVAL_MS,
  PROMPT_INTERVAL_MS,
  UPDATE_ENV_DISABLE,
  LEGACY_UPDATE_ENV_DISABLE,
  maybePromptForUpgrade,
  shouldRunUpdateCheck,
  shouldRefreshCache,
  shouldPrompt,
  resolveInstallPlan,
  findLocalProjectRoot,
  formatUpgradeQuestion,
  runInstallPlan,
  parseLatestVersion,
  compareVersions,
  normalizeVersion,
  versionParts,
  defaultCachePath
};
