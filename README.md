# cli-cockpit

`cli-cockpit` is a local dev cockpit for ports and git worktrees. It keeps a persistent map of which local project owns which TCP port, shows which ports are free, helps clean up stale dev servers before retrying your command, and lets you inspect and manage git worktrees for the current repository.

## Quick start

```bash
npm i cli-cockpit
npx cockpit
```

Or run a dev server with conflict handling:

```bash
npx cockpit run -- npm run dev
```

## What it does

- Scans local listening TCP ports and groups them by project root.
- Persists the last known owner of each port in a local state file.
- Highlights stale dev servers when the process looks orphaned.
- Lists likely free development ports.
- Wraps a command, detects port conflicts, and offers to kill a stale listener before rerunning.
- Shows git worktrees for the current repository, including dirty state, changed files, and branch sync.
- Runs worktree actions like add, lock, unlock, move, remove, cherry-pick, merge, rebase, and reset from the cockpit.
- Adds worktree safety checks (`remove --force` gating for dirty/unpushed/unmerged/in-progress states), bulk `sync` orchestration, stale scoring, and per-worktree task presets (`test`, `lint`, `build`).

## Install

`cli-cockpit` builds its Rust binary during `npm install`, so the machine needs a Rust toolchain.

Install in your project root (`<your-repo>/`) so everyone can run it with `npx`:

```bash
cd <your-repo>
npm i cli-cockpit
```

Then run it from that same project root:

```bash
npx cockpit
npx cli-cockpit
npx port
```

Optional global install:

```bash
npm install -g cli-cockpit
```

If you are developing from this repo directly, install from the repo root:

```bash
npm install -g .
```

## Typical workflow

1. Start the dashboard:

```bash
npx cockpit
```

2. If your app needs a specific port, preflight and run:

```bash
npx cockpit run --port 3000 -- npm run dev
```

3. If a stale process is blocking a port, release it:

```bash
npx cockpit release 3000
```

## Release flow

This repo uses a persistent, auto-updated release PR powered by Release Please.

- Every merge to `main` updates (or opens) a single release PR.
- That PR aggregates commit summaries and updates `CHANGELOG.md`.
- Merging that release PR creates the GitHub Release and tag.
- Publishing the release triggers npm publish automatically.

To get clean changelog entries and correct semver bumps, use Conventional Commits:

- `fix: ...` -> patch
- `feat: ...` -> minor
- `feat!: ...` or `BREAKING CHANGE:` -> major

Required repository secret:

- `NPM_TOKEN`: npm automation token with publish access.

## Commands

```bash
cockpit
cli-cockpit
npx cockpit
npx cli-cockpit
npx port
cockpit map
cockpit dashboard
port cockpit
cockpit map --interactive
cockpit map --plain
cockpit map --all
cockpit status 3000
cockpit inspect 3000
cockpit available --from 3000 --to 3100
cockpit free --from 3000 --to 3100
cockpit release 3000
cockpit kill 3000
cockpit run --port 3000 -- npm run dev
cockpit exec --port 3000 -- npm run dev
cockpit run -- npm run dev
```

## Dashboard view

Running `cockpit` (with no args) opens the interactive cockpit.
`cli-cockpit` and `port` are aliases for the same command.
Top-level aliases also work (for example `cockpit dashboard`, `cockpit free`, and `cockpit kill`).
`cockpit map` prints one-shot output unless you pass `--interactive`.
The cockpit has two views:

- `Ports` for the existing port map and session controls.
- `Worktrees` for the current git repository only. There is no automatic port-to-worktree linking in this version.

## Command model

- Global commands work from any view: `ports`, `worktrees`, `view ports`, `view worktrees`, `help`, `help ports`, `help worktrees`, `download`, `yes`, `no`, `quit`.
- In `Ports`, the command bar stays port-centric: `restart`, `kill`, `move`, `open`, `select`, `filter`, `sort`, `clear`.
- In `Worktrees`, bare verbs work directly, so you can type `new feature-a`, `switch main`, `remove --force`, `sync all --from main`, `task test`, `pick abc123`, `merge main`, or `rebase main`.
- If you prefer explicit scoping, the worktree prefixes `wt`, `worktree`, and `worktrees` are all accepted:
  - `wt new feature-a`
  - `worktree switch main`
  - `worktrees remove --force`
- Useful worktree aliases:
  - `new <branch>` -> create a new branch and a sibling worktree path automatically, for example `new feature/login` creates a worktree like `../feature-login`
  - `create` -> `add`
  - `checkout` and `co` -> `switch`
  - `delete` and `rm` -> `remove`
  - `cleanup` -> `prune`
  - `pick` -> `cherry-pick`
  - `cont` -> `continue`
  - `test`, `lint`, `build` -> task presets for the selected worktree (or pass a target)

- Use `Up` and `Down` to inspect sessions.
- Use `Tab` or `Shift+Tab` to switch between `Ports` and `Worktrees`.
- The TUI now repeats the `Tab -> Worktrees` or `Tab -> Ports` hint in the overview and list title so the view switch is always visible.
- Type commands in the bottom command box and press `Enter`.
- Use `?` or `F1` to toggle the in-app command menu.
- `Ctrl+C` quits the dashboard.
- Use `cockpit map --plain` when you want the original plain table output.

Supported dashboard commands:

- `restart`
- `restart 3000`
- `restart 3000 --port 3010`
- `quick stale` to kill stale sessions
- `quick old` to kill old sessions
- `quick restart-old` to restart old sessions
- `kill 3000`
- `kill pid 1234`
- `move 3000 3100`
- `move 3100` to move the currently selected session to a new port
- `filter stale`
- `filter health:up`
- `filter project:web`
- `sort health desc`
- `sort new-old` for newest first
- `sort old-new` for oldest first
- `clear` to reset filter and sort defaults
- `open`
- `open 3000`
- `download` to export the current snapshot to `./cli-cockpit-dashboard.json`
- `download reports/ports.json` to write an export relative to the current root
- `select 3000`
- `refresh`
- `help`
- `quit`

Supported worktree commands inside the cockpit:

- `worktrees`
- `filter dirty`
- `filter branch:feature`
- `sort state desc`
- `sort stale desc`
- `select feature`
- `open`
- `new feature-a`
- `new feature/login --from main`
- `add ../feature-a --branch feature-a --from main`
- `open feature`
- `switch main`
- `move ../feature-a-2`
- `lock --reason keep`
- `unlock`
- `remove --force`
- `sync all --from main`
- `sync feature-a --mode merge`
- `prune --dry-run`
- `task test`
- `task lint feature-a`
- `build`
- `pick abc123`
- `merge main`
- `rebase main`
- `reset --hard HEAD~1`
- `continue`
- `abort`
- `yes` / `no` to confirm or cancel destructive git actions

Worktree details show:

- branch or detached head
- ahead/behind against upstream
- ahead/behind against `origin/<branch>` and the base branch (`main`/`origin/main` when available)
- merged/unmerged status vs base branch
- lock and prunable state
- in-progress rebase/merge/cherry-pick state
- specific staged, unstaged, untracked, and conflicted file paths
- stale score (for cleanup ranking), last checkout age, and last commit subject/age
- latest task preset result (`test`/`lint`/`build`) with standardized status and output tail

The worktree list also includes a short changed-file preview on each row so you can spot which files moved without opening the full detail pane.

The cockpit now includes persistent per-view filter/sort state, visible-row health probing (`up:<code>`, `tcp`, `down`, `?`), explicit URL/file open actions, confirmation prompts for destructive git actions, and a denser list layout with better scanability.
Default list order is now `new -> old` by process age.
Port detail rows include PPID, TTY, first/last seen timestamps, project root, working directory, health details, and URL hints.
`download` still writes `./cli-cockpit-dashboard.json` by default, and the export now includes optional `repo` and `worktrees` sections when run inside a git repository.

## Shell helpers (optional)

Generate wrapper functions:

```bash
eval "$(cockpit hook zsh)"
```

That adds:

- `pmrun <command...>` to run a command through `cockpit run`
- `pmport <port> <command...>` to preflight a specific port before running the command

## Stale-port heuristic

`cli-cockpit` currently marks a listener as stale when either:

- its working directory no longer exists, or
- it looks like an orphaned dev server, has no terminal, is parented by pid `1`, and has been alive longer than the configured threshold

## Persistence

State is written to:

- macOS/Linux: `~/.local/share/cli-cockpit/state.json` when `XDG_DATA_HOME` is unset

The exact base path follows the platform data directory returned by the Node/Rust environment.

## Upgrade prompts

When a newer npm release is available, `cockpit`, `port`, and `cli-cockpit` ask a simple yes/no question before launching the binary:

```bash
cli-cockpit 0.x.y is available (current 0.a.b). Upgrade this project now? [Y/n]
```

Notes:

- The check only runs in an interactive terminal.
- If you answer `yes`, `cli-cockpit` runs the matching npm install command for you.
  - Local project install: `npm install cli-cockpit@latest`
  - Global install: `npm install -g cli-cockpit@latest`
- It is throttled and cached, so it does not hit npm on every command run.
- It is skipped when running from a source checkout of this repository.
- Set `CLI_COCKPIT_DISABLE_UPDATE_CHECK=1` to disable the prompt entirely.

## Open source

`cli-cockpit` is open source and community-driven. Issues, ideas, and pull requests are welcome.

## Contributing

Contributions of all sizes are welcome, including bug fixes, docs improvements, and feature work.

Basic workflow:

1. Fork the repo and create a branch for your change.
2. Make your update and test it locally.
3. Open a pull request with a clear description of what changed and why.

If you are unsure where to start, open an issue describing the problem or proposal first.
