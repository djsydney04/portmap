# portledger

`portledger` keeps a persistent map of which local project owns which TCP port, shows which ports are free, and helps clean up stale dev servers before retrying your command.

## What it does

- Scans local listening TCP ports and groups them by project root.
- Persists the last known owner of each port in a local state file.
- Highlights stale dev servers when the process looks orphaned.
- Lists likely free development ports.
- Wraps a command, detects port conflicts, and offers to kill a stale listener before rerunning.

## Install

`portledger` builds its Rust binary during `npm install`, so the machine needs a Rust toolchain.

Install in your project root (`<your-repo>/`) so everyone can run it with `npx`:

```bash
cd <your-repo>
npm install --save-dev portledger
```

Then run it from that same project root:

```bash
npx port
npx portledger
```

Optional global install:

```bash
npm install -g portledger
```

If you are developing from this repo directly, install from the repo root:

```bash
npm install -g .
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
port
portledger
port map
port map --interactive
port map --plain
port map --all
port status 3000
port available --from 3000 --to 3100
port release 3000
port run --port 3000 -- npm run dev
port run -- npm run dev
```

## Dashboard view

Running `port` (with no args) opens the interactive dashboard.
`portledger` is an alias for the same command.
`port map` prints one-shot output unless you pass `--interactive`.

- Use `Up` and `Down` to inspect sessions.
- Type commands in the bottom command box and press `Enter`.
- Use `?` or `F1` to toggle the in-app command menu.
- `Ctrl+C` quits the dashboard.
- Use `port map --plain` when you want the original plain table output.

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
- `download` to export the current snapshot to `./portledger-dashboard.json`
- `download reports/ports.json` to write an export relative to the current root
- `select 3000`
- `refresh`
- `help`
- `quit`

The dashboard now includes persistent filter/sort state, visible-row health probing (`up:<code>`, `tcp`, `down`, `?`), explicit URL open actions, and a denser list layout with better scanability.
Default list order is now `new -> old` by process age.
Detail rows include PPID, TTY, first/last seen timestamps, project root, working directory, health details, and URL hints.

## Shell helpers

Generate wrapper functions:

```bash
eval "$(port hook zsh)"
```

That adds:

- `pmrun <command...>` to run a command through `port run`
- `pmport <port> <command...>` to preflight a specific port before running the command

## Stale-port heuristic

`portledger` currently marks a listener as stale when either:

- its working directory no longer exists, or
- it looks like an orphaned dev server, has no terminal, is parented by pid `1`, and has been alive longer than the configured threshold

## Persistence

State is written to:

- macOS/Linux: `~/.local/share/portledger/state.json` when `XDG_DATA_HOME` is unset

The exact base path follows the platform data directory returned by the Node/Rust environment.

## Open source

`portledger` is open source and community-driven. Issues, ideas, and pull requests are welcome.

## Contributing

Contributions of all sizes are welcome, including bug fixes, docs improvements, and feature work.

Basic workflow:

1. Fork the repo and create a branch for your change.
2. Make your update and test it locally.
3. Open a pull request with a clear description of what changed and why.

If you are unsure where to start, open an issue describing the problem or proposal first.
