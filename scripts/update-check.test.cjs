const fs = require("fs");
const os = require("os");
const path = require("path");
const test = require("node:test");
const assert = require("node:assert/strict");

const {
  maybePromptForUpgrade,
  shouldRunUpdateCheck,
  shouldRefreshCache,
  shouldPrompt,
  resolveInstallPlan,
  formatUpgradeQuestion,
  parseLatestVersion,
  compareVersions,
  defaultCachePath,
  UPDATE_ENV_DISABLE,
  LEGACY_UPDATE_ENV_DISABLE,
  CHECK_INTERVAL_MS,
  PROMPT_INTERVAL_MS
} = require("./update-check.cjs");

test("compareVersions handles semver ordering", () => {
  assert.equal(compareVersions("0.3.0", "0.3.0"), 0);
  assert.equal(compareVersions("0.3.1", "0.3.0"), 1);
  assert.equal(compareVersions("1.0.0", "0.9.9"), 1);
  assert.equal(compareVersions("0.3.0", "0.10.0"), -1);
  assert.equal(compareVersions("v0.3.0-beta.1", "0.3.0"), 0);
});

test("parseLatestVersion supports npm json output", () => {
  assert.equal(parseLatestVersion('"0.4.0"\n'), "0.4.0");
  assert.equal(parseLatestVersion('["0.3.0","0.4.0"]\n'), "0.4.0");
  assert.equal(parseLatestVersion("v0.5.0\n"), "0.5.0");
});

test("shouldRunUpdateCheck respects tty, opt-out, and source checkouts", () => {
  assert.equal(
    shouldRunUpdateCheck({
      stdin: { isTTY: true },
      stderr: { isTTY: true },
      env: {}
    }),
    true
  );
  assert.equal(
    shouldRunUpdateCheck({
      stdin: { isTTY: false },
      stderr: { isTTY: true },
      env: {}
    }),
    false
  );
  assert.equal(
    shouldRunUpdateCheck({
      stdin: { isTTY: true },
      stderr: { isTTY: true },
      env: { [UPDATE_ENV_DISABLE]: "1" }
    }),
    false
  );
  assert.equal(
    shouldRunUpdateCheck({
      stdin: { isTTY: true },
      stderr: { isTTY: true },
      env: { [LEGACY_UPDATE_ENV_DISABLE]: "1" }
    }),
    false
  );

  const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), "cli-cockpit-source-"));
  fs.mkdirSync(path.join(tempRoot, ".git"));
  assert.equal(
    shouldRunUpdateCheck({
      stdin: { isTTY: true },
      stderr: { isTTY: true },
      env: {},
      root: tempRoot
    }),
    false
  );
});

test("cache refresh and prompt intervals are throttled", () => {
  const now = 1_700_000_000_000;
  assert.equal(shouldRefreshCache({}, now), true);
  assert.equal(shouldRefreshCache({ checkedAt: now }, now), false);
  assert.equal(
    shouldRefreshCache({ checkedAt: now - CHECK_INTERVAL_MS - 1 }, now),
    true
  );

  assert.equal(shouldPrompt({}, "0.4.0", now), true);
  assert.equal(
    shouldPrompt({ promptedVersion: "0.4.0", promptedAt: now }, "0.4.0", now),
    false
  );
  assert.equal(
    shouldPrompt(
      { promptedVersion: "0.4.0", promptedAt: now - PROMPT_INTERVAL_MS - 1 },
      "0.4.0",
      now
    ),
    true
  );
});

test("resolveInstallPlan prefers local project install when package matches cwd ancestry", () => {
  const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), "cli-cockpit-local-"));
  const projectRoot = path.join(tempRoot, "project");
  const packageRoot = path.join(projectRoot, "node_modules", "cli-cockpit");
  const nestedCwd = path.join(projectRoot, "apps", "web");

  fs.mkdirSync(packageRoot, { recursive: true });
  fs.mkdirSync(nestedCwd, { recursive: true });

  const plan = resolveInstallPlan({
    packageName: "cli-cockpit",
    root: packageRoot,
    cwd: nestedCwd
  });

  assert.equal(plan.label, "project");
  assert.equal(plan.cwd, fs.realpathSync(projectRoot));
  assert.deepEqual(plan.args, ["install", "cli-cockpit@latest"]);
});

test("formatUpgradeQuestion stays yes-no and scope aware", () => {
  assert.match(
    formatUpgradeQuestion({
      packageName: "cli-cockpit",
      latestVersion: "0.4.0",
      currentVersion: "0.3.0",
      installPlan: { label: "project" }
    }),
    /Upgrade this project now\?/
  );
  assert.match(
    formatUpgradeQuestion({
      packageName: "cli-cockpit",
      latestVersion: "0.4.0",
      currentVersion: "0.3.0",
      installPlan: { label: "global" }
    }),
    /Upgrade the global install now\?/
  );
});

test("maybePromptForUpgrade runs install when user accepts", async () => {
  const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), "cli-cockpit-update-"));
  const cachePath = path.join(tempRoot, "update.json");
  const events = [];

  const result = await maybePromptForUpgrade({
    packageName: "cli-cockpit",
    currentVersion: "0.3.0",
    root: path.join(tempRoot, "pkg"),
    cwd: tempRoot,
    now: 1000,
    stdin: { isTTY: true },
    stderr: { isTTY: true },
    env: {},
    cachePath,
    execNpmView() {
      return "0.4.0";
    },
    async askYesNo({ question }) {
      events.push(["question", question]);
      return true;
    },
    runInstall(plan) {
      events.push(["install", plan]);
      return { ok: true };
    }
  });

  assert.equal(result.upgraded, true);
  assert.equal(events[0][0], "question");
  assert.equal(events[1][0], "install");
  assert.match(events[0][1], /Upgrade the global install now\?/);
});

test("maybePromptForUpgrade skips install when user declines", async () => {
  const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), "cli-cockpit-update-"));
  const cachePath = path.join(tempRoot, "update.json");
  let installCalled = false;

  const result = await maybePromptForUpgrade({
    packageName: "cli-cockpit",
    currentVersion: "0.3.0",
    root: path.join(tempRoot, "pkg"),
    cwd: tempRoot,
    now: 1000,
    stdin: { isTTY: true },
    stderr: { isTTY: true },
    env: {},
    cachePath,
    execNpmView() {
      return "0.4.0";
    },
    async askYesNo() {
      return false;
    },
    runInstall() {
      installCalled = true;
      return { ok: true };
    }
  });

  assert.equal(result.upgraded, false);
  assert.equal(result.declined, true);
  assert.equal(installCalled, false);
});

test("defaultCachePath includes package name", () => {
  const value = defaultCachePath("cli-cockpit");
  assert.match(value, /cli-cockpit/);
  assert.match(value, /update-check\.json$/);
});
