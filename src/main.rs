use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{self, IsTerminal, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, ExitCode, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod worktree;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use comfy_table::{
    modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Cell, CellAlignment, Color,
    ContentArrangement, Table,
};
use crossterm::{
    cursor::{MoveTo, Show},
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    terminal::{
        self, disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use dialoguer::Confirm;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use worktree::{
    active_operation, discover_current_repo, run_git, RepoSnapshot, WorktreeOperation,
    WorktreeRecord,
};

const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(30 * 60);
const DEFAULT_DASHBOARD_REFRESH: Duration = Duration::from_secs(3);
const DEFAULT_AVAILABLE_FROM: u16 = 3000;
const DEFAULT_AVAILABLE_TO: u16 = 3999;
const DEFAULT_AVAILABLE_COUNT: usize = 12;
const RELEASE_HISTORY_LIMIT: usize = 8;
const CAPTURE_LIMIT: usize = 64 * 1024;
const DEFAULT_EXPORT_FILENAME: &str = "cli-cockpit-dashboard.json";
const DASHBOARD_MOVE_WAIT: Duration = Duration::from_secs(12);
const QUICK_ACTION_OLD_AFTER: Duration = Duration::from_secs(2 * 60 * 60);

static DEV_SERVER_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?ix)
        \b(
            vite|next|nuxt|astro|remix|webpack|parcel|snowpack|svelte|storybook|
            deno\s+task|npm\s+run\s+dev|pnpm\s+dev|yarn\s+dev|bun\s+run\s+dev|
            cargo\s+run|cargo-watch|air|uvicorn|gunicorn|flask|django|rails\s+s|
            mix\s+phx\.server|python\s+-m\s+http\.server|http-server|serve
        )\b",
    )
    .expect("valid dev-server regex")
});

static PORT_FROM_ERROR_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?ix)
        (?:
            port \s+ (?P<label>\d{2,5}) \s+ (?:is\s+already\s+)?in\s+use |
            eaddrinuse [^\n]* : (?P<suffix>\d{2,5}) |
            already \s+ in \s+ use [^\n]* : (?P<trailing>\d{2,5})
        )",
    )
    .expect("valid bind-error regex")
});

static PROJECT_MARKERS: &[&str] = &[
    ".git",
    "package.json",
    "pnpm-workspace.yaml",
    "yarn.lock",
    "bun.lockb",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "requirements.txt",
    "mix.exs",
    "deno.json",
    "deno.jsonc",
    "vite.config.ts",
    "vite.config.js",
    "turbo.json",
];

#[derive(Parser, Debug)]
#[command(
    name = "cockpit",
    bin_name = "cockpit",
    version,
    about = "Port and worktree cockpit for local development",
    long_about = "Port and worktree cockpit for local development.\n\nRunning `cockpit` with no command opens the interactive dashboard.",
    after_help = "Examples:\n  cockpit                              Open the interactive dashboard\n  cockpit dashboard                    Open the interactive dashboard via explicit command\n  cockpit map --plain                  Print one-shot table output\n  cockpit status 3000                  Inspect one port and owner history\n  cockpit free --from 3000 --to 3100   Show likely free ports\n  cockpit kill 3000                    Release a busy port\n  cockpit run --port 3000 -- npm run dev"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show the current port map
    #[command(visible_aliases = ["dashboard", "cockpit"])]
    Map(MapArgs),
    /// Inspect one port and show its current or last known owner
    #[command(visible_aliases = ["info", "inspect"])]
    Status(StatusArgs),
    /// Find free ports in a range
    #[command(visible_aliases = ["free", "find-free"])]
    Available(AvailableArgs),
    /// Kill the process currently listening on a port
    #[command(visible_aliases = ["kill", "stop"])]
    Release(ReleaseArgs),
    /// Run a command, detect port conflicts, and offer stale-port cleanup
    #[command(visible_aliases = ["exec", "wrap"])]
    Run(RunArgs),
    /// Print shell helpers for wrapping dev commands with cockpit
    #[command(visible_aliases = ["shell", "helpers"])]
    Hook(HookArgs),
}

#[derive(Args, Debug, Clone)]
struct MapArgs {
    /// Include recently released ports from the persisted history
    #[arg(long)]
    all: bool,

    /// Open the interactive dashboard view
    #[arg(long)]
    interactive: bool,

    /// Print the one-shot table view instead of the interactive dashboard
    #[arg(long)]
    plain: bool,

    /// Refresh interval for the interactive dashboard
    #[arg(long, default_value = "3s", value_parser = parse_duration)]
    refresh_every: Duration,

    /// Age threshold before an orphaned dev server is treated as stale
    #[arg(long, default_value = "30m", value_parser = parse_duration)]
    stale_after: Duration,
}

#[derive(Args, Debug, Clone)]
struct StatusArgs {
    port: u16,

    /// Age threshold before an orphaned dev server is treated as stale
    #[arg(long, default_value = "30m", value_parser = parse_duration)]
    stale_after: Duration,
}

#[derive(Args, Debug, Clone)]
struct AvailableArgs {
    #[arg(long, default_value_t = DEFAULT_AVAILABLE_FROM)]
    from: u16,

    #[arg(long, default_value_t = DEFAULT_AVAILABLE_TO)]
    to: u16,

    #[arg(long, default_value_t = DEFAULT_AVAILABLE_COUNT)]
    count: usize,

    /// Age threshold before an orphaned dev server is treated as stale
    #[arg(long, default_value = "30m", value_parser = parse_duration)]
    stale_after: Duration,
}

#[derive(Args, Debug, Clone)]
struct ReleaseArgs {
    port: u16,

    /// Skip confirmation
    #[arg(long)]
    yes: bool,

    /// Age threshold before an orphaned dev server is treated as stale
    #[arg(long, default_value = "30m", value_parser = parse_duration)]
    stale_after: Duration,
}

#[derive(Args, Debug, Clone)]
struct RunArgs {
    /// Preflight a specific port before running the command
    #[arg(long)]
    port: Option<u16>,

    /// Skip interactive confirmation before killing a stale process
    #[arg(long)]
    yes: bool,

    /// Age threshold before an orphaned dev server is treated as stale
    #[arg(long, default_value = "30m", value_parser = parse_duration)]
    stale_after: Duration,

    #[arg(last = true, required = true)]
    command: Vec<String>,
}

#[derive(Args, Debug, Clone)]
struct HookArgs {
    #[arg(value_enum)]
    shell: Shell,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Shell {
    Bash,
    Zsh,
    Fish,
}

#[derive(Debug, Clone)]
struct PortSnapshot {
    port: u16,
    owners: Vec<ProcessRecord>,
}

#[derive(Debug, Clone)]
struct ProcessRecord {
    pid: i32,
    ppid: Option<i32>,
    tty: Option<String>,
    command_line: String,
    cwd: Option<PathBuf>,
    project_root: Option<PathBuf>,
    project_name: String,
    age: Option<Duration>,
    stale: bool,
    stale_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct RawListener {
    pid: i32,
    command_name: String,
    port: u16,
}

#[derive(Debug, Clone)]
struct ProcessMeta {
    ppid: Option<i32>,
    tty: Option<String>,
    age: Option<Duration>,
    command_line: String,
    cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StateFile {
    version: u8,
    updated_at_epoch: i64,
    ports: BTreeMap<u16, PersistedPortRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedPortRecord {
    port: u16,
    project_name: Option<String>,
    project_root: Option<PathBuf>,
    command_line: Option<String>,
    pid: Option<i32>,
    first_seen_epoch: i64,
    last_seen_epoch: i64,
    released_at_epoch: Option<i64>,
    last_status: PersistedStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PersistedStatus {
    Listening,
    Released,
}

impl Default for PersistedPortRecord {
    fn default() -> Self {
        Self {
            port: 0,
            project_name: None,
            project_root: None,
            command_line: None,
            pid: None,
            first_seen_epoch: 0,
            last_seen_epoch: 0,
            released_at_epoch: None,
            last_status: PersistedStatus::Released,
        }
    }
}

#[derive(Debug)]
struct CommandOutcome {
    status: ExitStatus,
    stderr: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();

    match cli.command {
        None => cmd_map(MapArgs {
            all: false,
            interactive: true,
            plain: false,
            refresh_every: DEFAULT_DASHBOARD_REFRESH,
            stale_after: DEFAULT_STALE_AFTER,
        }),
        Some(Commands::Map(args)) => cmd_map(args),
        Some(Commands::Status(args)) => cmd_status(args),
        Some(Commands::Available(args)) => cmd_available(args),
        Some(Commands::Release(args)) => cmd_release(args),
        Some(Commands::Run(args)) => cmd_run(args),
        Some(Commands::Hook(args)) => cmd_hook(args),
    }
}

fn cmd_map(args: MapArgs) -> Result<ExitCode> {
    if args.interactive
        && !args.plain
        && !args.all
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
    {
        return run_dashboard(args);
    }

    cmd_map_plain(args)
}

fn cmd_map_plain(args: MapArgs) -> Result<ExitCode> {
    let snapshot = refresh_snapshot(args.stale_after)?;

    if snapshot.active.is_empty() {
        println!("No listening ports found.");
    } else {
        println!("Active listeners");
        println!();
        print_active_table(&snapshot.active);
    }

    let free = free_ports(
        DEFAULT_AVAILABLE_FROM,
        DEFAULT_AVAILABLE_TO,
        &snapshot.active,
        8,
    );
    if !free.is_empty() {
        println!();
        println!(
            "Likely free dev ports: {}",
            free.iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    if args.all {
        let recent = recent_released_records(&snapshot.state, RELEASE_HISTORY_LIMIT);
        if !recent.is_empty() {
            println!();
            println!("Recently released");
            println!();
            print_released_table(&recent);
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn cmd_status(args: StatusArgs) -> Result<ExitCode> {
    let snapshot = refresh_snapshot(args.stale_after)?;

    if let Some(active) = snapshot.active.iter().find(|entry| entry.port == args.port) {
        println!("Port {} is in use.", args.port);
        println!();
        print_active_table(std::slice::from_ref(active));
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(record) = snapshot.state.ports.get(&args.port) {
        println!("Port {} is free.", args.port);
        if let Some(owner) = record.project_name.as_deref() {
            println!(
                "Last known owner: {} ({})",
                owner,
                relative_time(record.last_seen_epoch)
            );
        }
        if let Some(command) = record.command_line.as_deref() {
            println!("Last command: {}", trim_middle(command, 100));
        }
        if let Some(root) = record.project_root.as_ref() {
            println!("Project root: {}", display_path(root));
        }
    } else {
        println!(
            "Port {} is free and has no recorded history yet.",
            args.port
        );
    }

    Ok(ExitCode::SUCCESS)
}

fn cmd_available(args: AvailableArgs) -> Result<ExitCode> {
    if args.from > args.to {
        bail!("--from must be less than or equal to --to");
    }

    let snapshot = refresh_snapshot(args.stale_after)?;
    let available = free_ports(args.from, args.to, &snapshot.active, args.count);

    if available.is_empty() {
        println!("No free ports found between {} and {}.", args.from, args.to);
        return Ok(ExitCode::SUCCESS);
    }

    let mut table = build_table();
    table.set_header(vec!["Port", "Last owner", "Last seen"]);

    for port in available {
        let record = snapshot.state.ports.get(&port);
        table.add_row(vec![
            Cell::new(port)
                .fg(Color::Green)
                .set_alignment(CellAlignment::Right),
            Cell::new(
                record
                    .and_then(|entry| entry.project_name.clone())
                    .unwrap_or_else(|| "unused".to_string()),
            ),
            Cell::new(
                record
                    .map(|entry| relative_time(entry.last_seen_epoch))
                    .unwrap_or_else(|| "never".to_string()),
            ),
        ]);
    }

    println!("{table}");
    Ok(ExitCode::SUCCESS)
}

fn cmd_release(args: ReleaseArgs) -> Result<ExitCode> {
    let snapshot = refresh_snapshot(args.stale_after)?;
    let active = snapshot
        .active
        .iter()
        .find(|entry| entry.port == args.port)
        .cloned();

    let Some(active) = active else {
        println!("Port {} is already free.", args.port);
        return Ok(ExitCode::SUCCESS);
    };

    println!("Port {} is currently in use:", args.port);
    println!();
    print_active_table(std::slice::from_ref(&active));

    let pids = unique_pids(&active);
    if !args.yes && !confirm(&format!("Send SIGTERM to {}?", describe_pids(&pids)))? {
        println!("Aborted.");
        return Ok(ExitCode::from(1));
    }

    terminate_processes(&pids)?;
    if wait_for_port_to_clear(args.port, Duration::from_secs(2), args.stale_after)?.is_some() {
        bail!("port {} is still occupied after SIGTERM", args.port);
    }

    println!("Port {} is free.", args.port);
    Ok(ExitCode::SUCCESS)
}

fn cmd_run(args: RunArgs) -> Result<ExitCode> {
    if args.command.is_empty() {
        bail!("no command provided");
    }

    if let Some(port) = args.port {
        let snapshot = refresh_snapshot(args.stale_after)?;
        if let Some(active) = snapshot.active.iter().find(|entry| entry.port == port) {
            let resolved = maybe_resolve_conflict(active, port, args.yes, args.stale_after)?;
            if !resolved {
                return Ok(ExitCode::from(1));
            }
        }
    }

    let outcome = spawn_and_capture(&args.command)?;
    if outcome.status.success() {
        return Ok(exit_code_from_status(outcome.status));
    }

    let detected_port = args
        .port
        .or_else(|| detect_conflicting_port(&outcome.stderr));

    if let Some(port) = detected_port {
        let snapshot = refresh_snapshot(args.stale_after)?;
        if let Some(active) = snapshot.active.iter().find(|entry| entry.port == port) {
            let resolved = maybe_resolve_conflict(active, port, args.yes, args.stale_after)?;
            if resolved {
                let retry = spawn_and_capture(&args.command)?;
                return Ok(exit_code_from_status(retry.status));
            }
        }
    }

    Ok(exit_code_from_status(outcome.status))
}

fn cmd_hook(args: HookArgs) -> Result<ExitCode> {
    match args.shell {
        Shell::Bash | Shell::Zsh => {
            println!(
                r#"pmrun() {{
  command cockpit run -- "$@"
}}

pmport() {{
  local port="$1"
  shift
  command cockpit run --port "$port" -- "$@"
}}"#
            );
        }
        Shell::Fish => {
            println!(
                r#"function pmrun
  command cockpit run -- $argv
end

function pmport
  set -l port $argv[1]
  set -e argv[1]
  command cockpit run --port $port -- $argv
end"#
            );
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn maybe_resolve_conflict(
    active: &PortSnapshot,
    port: u16,
    auto_confirm: bool,
    stale_after: Duration,
) -> Result<bool> {
    println!();
    println!("Port {} is already in use:", port);
    println!();
    print_active_table(std::slice::from_ref(active));

    let pids = unique_pids(active);
    let stale_reasons: Vec<_> = active
        .owners
        .iter()
        .filter_map(|owner| owner.stale_reason.as_ref())
        .cloned()
        .collect();

    if active.owners.iter().all(|owner| owner.stale) {
        println!();
        println!("This looks stale: {}", stale_reasons.join("; "));

        if auto_confirm
            || confirm(&format!(
                "Terminate {} and rerun the command?",
                describe_pids(&pids)
            ))?
        {
            terminate_processes(&pids)?;
            if wait_for_port_to_clear(port, Duration::from_secs(2), stale_after)?.is_some() {
                bail!("port {} is still occupied after SIGTERM", port);
            }

            println!("Freed port {}.", port);
            println!();
            return Ok(true);
        }

        println!("Left the stale process running.");
        return Ok(false);
    }

    println!();
    println!(
        "Portledger left port {} alone because it does not look stale. Use `cockpit release {}` if you want to stop it manually.",
        port, port
    );
    Ok(false)
}

fn refresh_snapshot(stale_after: Duration) -> Result<Snapshot> {
    let active = discover_active_ports(stale_after)?;
    let mut state = load_state()?;
    merge_state(&mut state, &active);
    save_state(&state)?;
    Ok(Snapshot { active, state })
}

#[derive(Debug, Clone)]
struct Snapshot {
    active: Vec<PortSnapshot>,
    state: StateFile,
}

#[derive(Debug, Clone)]
struct DashboardRow {
    port: u16,
    owner: ProcessRecord,
    owner_count: usize,
    history: Option<PersistedPortRecord>,
}

#[derive(Debug, Clone)]
struct DashboardState {
    snapshot: Snapshot,
    all_rows: Vec<DashboardRow>,
    rows: Vec<DashboardRow>,
    selected: usize,
    message: DashboardMessage,
    filter: Option<DashboardFilter>,
    sort: DashboardSort,
    health_cache: HashMap<(u16, i32), HealthStatus>,
}

#[derive(Debug, Clone)]
struct WorktreeDashboardState {
    repo: Option<RepoSnapshot>,
    rows: Vec<WorktreeRow>,
    all_rows: Vec<WorktreeRow>,
    selected: usize,
    filter: Option<WorktreeFilter>,
    sort: WorktreeSort,
    message: DashboardMessage,
    last_task: Option<WorktreeTaskResult>,
}

#[derive(Debug, Clone)]
struct WorktreeRow {
    worktree: WorktreeRecord,
}

#[derive(Debug, Clone)]
struct CockpitState {
    view: CockpitView,
    ports: DashboardState,
    worktrees: WorktreeDashboardState,
    input: String,
    show_command_menu: bool,
    pending_confirmation: Option<PendingConfirmation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CockpitView {
    Ports,
    Worktrees,
}

#[derive(Debug, Clone)]
struct PendingConfirmation {
    prompt: String,
    action: ConfirmableAction,
}

#[derive(Debug, Clone)]
enum ConfirmableAction {
    RemoveWorktree {
        target: String,
        force: bool,
    },
    PruneWorktrees {
        dry_run: bool,
    },
    CherryPickWorktree {
        target: String,
        commits: Vec<String>,
    },
    MergeWorktree {
        target: String,
        reference: String,
    },
    RebaseWorktree {
        target: String,
        reference: String,
    },
    ResetWorktree {
        target: String,
        mode: ResetMode,
        reference: String,
    },
}

#[derive(Debug, Clone)]
struct DashboardMessage {
    level: MessageLevel,
    text: String,
}

#[derive(Debug, Clone, Copy)]
enum MessageLevel {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DashboardCommand {
    Help,
    Refresh,
    Kill(Vec<String>),
    Restart(RestartRequest),
    Quick(QuickAction),
    Move { from: Option<u16>, to: u16 },
    Download(Option<String>),
    Filter(String),
    Sort(DashboardSort),
    Clear,
    Open(Option<String>),
    Select(String),
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RestartRequest {
    target: Option<String>,
    port_override: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuickAction {
    KillStale,
    KillOld,
    RestartOld,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DashboardSort {
    key: DashboardSortKey,
    direction: SortDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardSortKey {
    Port,
    Project,
    Age,
    State,
    Health,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone)]
enum DashboardFilter {
    Stale,
    Active,
    Health(HealthFilter),
    Port(u16),
    ProjectContains(String),
    CommandContains(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthFilter {
    Up,
    Tcp,
    Down,
    Unknown,
}

#[derive(Debug, Clone)]
enum HealthStatus {
    Unknown,
    Up(u16),
    Tcp,
    Down(String),
}

#[derive(Debug, Clone)]
enum WorktreeFilter {
    Dirty,
    Clean,
    Conflicted,
    Locked,
    Prunable,
    Detached,
    BranchContains(String),
    PathContains(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorktreeSort {
    key: WorktreeSortKey,
    direction: SortDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorktreeSortKey {
    Path,
    Branch,
    Sync,
    Changes,
    State,
    Stale,
}

#[derive(Debug, Clone)]
enum WorktreeCommand {
    Add(WorktreeAddRequest),
    Open(Option<String>),
    Switch(WorktreeSwitchRequest),
    Move(WorktreeMoveRequest),
    Lock(WorktreeLockRequest),
    Unlock(Option<String>),
    Remove(WorktreeRemoveRequest),
    Prune { dry_run: bool },
    CherryPick(WorktreeCherryPickRequest),
    Merge(WorktreeRefRequest),
    Rebase(WorktreeRefRequest),
    Reset(WorktreeResetRequest),
    Continue(Option<String>),
    Abort(Option<String>),
    Sync(WorktreeSyncRequest),
    Task(WorktreeTaskRequest),
}

#[derive(Debug, Clone)]
struct WorktreeAddRequest {
    path: String,
    branch: Option<String>,
    from_ref: Option<String>,
    detach: bool,
    no_checkout: bool,
    lock: bool,
    implicit_path_from_branch: bool,
}

#[derive(Debug, Clone)]
struct WorktreeSwitchRequest {
    reference: String,
    target: Option<String>,
    create: bool,
    track: bool,
}

#[derive(Debug, Clone)]
struct WorktreeMoveRequest {
    new_path: String,
    target: Option<String>,
}

#[derive(Debug, Clone)]
struct WorktreeLockRequest {
    target: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Clone)]
struct WorktreeRemoveRequest {
    target: Option<String>,
    force: bool,
}

#[derive(Debug, Clone)]
struct WorktreeCherryPickRequest {
    commits: Vec<String>,
    target: Option<String>,
}

#[derive(Debug, Clone)]
struct WorktreeRefRequest {
    reference: String,
    target: Option<String>,
}

#[derive(Debug, Clone)]
struct WorktreeResetRequest {
    mode: ResetMode,
    reference: String,
    target: Option<String>,
}

#[derive(Debug, Clone)]
struct WorktreeSyncRequest {
    target: Option<String>,
    from_ref: Option<String>,
    mode: WorktreeSyncMode,
    all: bool,
    include_dirty: bool,
    include_main: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorktreeSyncMode {
    Rebase,
    Merge,
}

#[derive(Debug, Clone)]
struct WorktreeTaskRequest {
    preset: WorktreeTaskPreset,
    target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorktreeTaskPreset {
    Test,
    Lint,
    Build,
}

#[derive(Debug, Clone)]
struct WorktreeTaskResult {
    target_path: PathBuf,
    preset: WorktreeTaskPreset,
    command: String,
    success: bool,
    exit_code: i32,
    duration: Duration,
    output_tail: Vec<String>,
    finished_at_epoch: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardExport {
    exported_at_epoch: i64,
    cwd: PathBuf,
    stale_after_seconds: u64,
    summary: DashboardExportSummary,
    free_ports: Vec<u16>,
    active: Vec<DashboardExportPort>,
    recent_released: Vec<DashboardExportReleased>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<DashboardExportRepo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktrees: Option<Vec<DashboardExportWorktree>>,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardExportSummary {
    ports: usize,
    processes: usize,
    stale: usize,
    projects: usize,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardExportPort {
    port: u16,
    owner_count: usize,
    first_seen_epoch: Option<i64>,
    last_seen_epoch: Option<i64>,
    owners: Vec<DashboardExportOwner>,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardExportOwner {
    pid: i32,
    ppid: Option<i32>,
    tty: Option<String>,
    command_line: String,
    cwd: Option<PathBuf>,
    project_root: Option<PathBuf>,
    project_name: String,
    age_seconds: Option<u64>,
    stale: bool,
    stale_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardExportReleased {
    port: u16,
    project_name: Option<String>,
    project_root: Option<PathBuf>,
    command_line: Option<String>,
    pid: Option<i32>,
    first_seen_epoch: i64,
    last_seen_epoch: i64,
    released_at_epoch: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardExportRepo {
    root: PathBuf,
    common_dir: PathBuf,
    worktree_count: usize,
    dirty: usize,
    locked: usize,
    prunable: usize,
    detached: usize,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardExportWorktree {
    path: PathBuf,
    is_main: bool,
    branch: Option<String>,
    detached: bool,
    locked: bool,
    lock_reason: Option<String>,
    prunable: bool,
    prunable_reason: Option<String>,
    upstream: Option<String>,
    ahead: u32,
    behind: u32,
    origin_ref: Option<String>,
    origin_ahead: u32,
    origin_behind: u32,
    base_ref: Option<String>,
    base_ahead: u32,
    base_behind: u32,
    merged_into_base: Option<bool>,
    head_oid: String,
    git_dir: Option<PathBuf>,
    operations: Vec<String>,
    staged: Vec<String>,
    unstaged: Vec<String>,
    untracked: Vec<String>,
    conflicted: Vec<String>,
    last_commit_epoch: Option<i64>,
    last_commit_subject: Option<String>,
    last_checkout_epoch: Option<i64>,
    stale_score: u32,
}

fn discover_active_ports(stale_after: Duration) -> Result<Vec<PortSnapshot>> {
    let listeners = query_lsof_listeners()?;
    if listeners.is_empty() {
        return Ok(Vec::new());
    }

    let pids: Vec<i32> = listeners.iter().map(|listener| listener.pid).collect();
    let process_map = query_processes(&pids)?;

    let mut grouped: BTreeMap<u16, Vec<ProcessRecord>> = BTreeMap::new();
    let mut seen = HashSet::new();

    for listener in listeners {
        if !seen.insert((listener.port, listener.pid)) {
            continue;
        }

        let meta = process_map
            .get(&listener.pid)
            .cloned()
            .unwrap_or_else(|| ProcessMeta {
                ppid: None,
                tty: None,
                age: None,
                command_line: listener.command_name.clone(),
                cwd: None,
            });

        let project_root = meta.cwd.as_deref().and_then(guess_project_root);
        let project_name = project_root
            .as_deref()
            .and_then(project_name_from_root)
            .unwrap_or_else(|| listener.command_name.clone());
        let stale_reason = determine_stale_reason(&meta, stale_after);
        let record = ProcessRecord {
            pid: listener.pid,
            ppid: meta.ppid,
            tty: meta.tty.clone(),
            command_line: meta.command_line.clone(),
            cwd: meta.cwd.clone(),
            project_root: project_root.clone(),
            project_name,
            age: meta.age,
            stale: stale_reason.is_some(),
            stale_reason,
        };

        grouped.entry(listener.port).or_default().push(record);
    }

    Ok(grouped
        .into_iter()
        .map(|(port, mut owners)| {
            owners.sort_by_key(|owner| owner.pid);
            PortSnapshot { port, owners }
        })
        .collect())
}

fn query_lsof_listeners() -> Result<Vec<RawListener>> {
    let output = Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"])
        .output()
        .context("failed to execute lsof")?;

    if !output.status.success() && output.stdout.is_empty() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8(output.stdout).context("lsof returned non-utf8 output")?;
    Ok(parse_lsof_output(&stdout))
}

fn parse_lsof_output(stdout: &str) -> Vec<RawListener> {
    let mut current_pid = None;
    let mut current_command = None;
    let mut listeners = Vec::new();

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }

        let (prefix, value) = line.split_at(1);
        match prefix {
            "p" => {
                current_pid = value.parse::<i32>().ok();
            }
            "c" => {
                current_command = Some(value.to_string());
            }
            "n" => {
                if let (Some(pid), Some(command_name), Some(port)) =
                    (current_pid, current_command.clone(), extract_port(value))
                {
                    listeners.push(RawListener {
                        pid,
                        command_name,
                        port,
                    });
                }
            }
            _ => {}
        }
    }

    listeners
}

fn query_processes(pids: &[i32]) -> Result<HashMap<i32, ProcessMeta>> {
    let mut unique = BTreeSet::new();
    unique.extend(pids.iter().copied());

    if unique.is_empty() {
        return Ok(HashMap::new());
    }

    let pid_list = unique
        .iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(",");

    let output = Command::new("ps")
        .args(["-o", "pid=,ppid=,tty=,etime=,command=", "-p", &pid_list])
        .output()
        .context("failed to execute ps")?;

    let stdout = String::from_utf8(output.stdout).context("ps returned non-utf8 output")?;
    let mut processes = HashMap::new();

    for line in stdout.lines() {
        if let Some((pid, ppid, tty, age, command_line)) = parse_ps_line(line) {
            let cwd = query_cwd(pid).ok().flatten();
            processes.insert(
                pid,
                ProcessMeta {
                    ppid,
                    tty,
                    age,
                    command_line,
                    cwd,
                },
            );
        }
    }

    Ok(processes)
}

type ParsedPsLine = (i32, Option<i32>, Option<String>, Option<Duration>, String);

fn parse_ps_line(line: &str) -> Option<ParsedPsLine> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<i32>().ok()?;
    let ppid = parts.next().and_then(|value| value.parse::<i32>().ok());
    let tty = parts.next().map(|value| value.to_string());
    let elapsed = parts.next().and_then(parse_elapsed_time);
    let command_start = nth_whitespace_index(line, 4)?;
    let command_line = line[command_start..].trim().to_string();
    Some((pid, ppid, tty, elapsed, command_line))
}

fn nth_whitespace_index(line: &str, fields: usize) -> Option<usize> {
    let mut transitions = 0usize;
    let mut in_whitespace = true;

    for (index, ch) in line.char_indices() {
        if ch.is_whitespace() {
            in_whitespace = true;
            continue;
        }

        if in_whitespace {
            transitions += 1;
            if transitions == fields + 1 {
                return Some(index);
            }
        }

        in_whitespace = false;
    }

    None
}

fn query_cwd(pid: i32) -> Result<Option<PathBuf>> {
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .with_context(|| format!("failed to query cwd for pid {}", pid))?;

    if !output.status.success() && output.stdout.is_empty() {
        return Ok(None);
    }

    let stdout = String::from_utf8(output.stdout).context("cwd lsof returned non-utf8 output")?;
    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix('n') {
            return Ok(Some(PathBuf::from(path)));
        }
    }

    Ok(None)
}

fn determine_stale_reason(meta: &ProcessMeta, stale_after: Duration) -> Option<String> {
    if let Some(cwd) = meta.cwd.as_ref() {
        if !cwd.exists() {
            return Some("working directory no longer exists".to_string());
        }
    }

    let no_tty = meta.tty.as_deref().map(is_tty_missing).unwrap_or(true);
    let orphaned = matches!(meta.ppid, Some(1));
    let old_enough = meta.age.unwrap_or_default() >= stale_after;
    let dev_like = DEV_SERVER_PATTERN.is_match(&meta.command_line);

    if dev_like && no_tty && orphaned && old_enough {
        return Some("orphaned dev server without a terminal".to_string());
    }

    None
}

fn is_tty_missing(tty: &str) -> bool {
    matches!(tty, "?" | "??" | "-" | "")
}

fn guess_project_root(start: &Path) -> Option<PathBuf> {
    for candidate in start.ancestors() {
        if PROJECT_MARKERS
            .iter()
            .any(|marker| candidate.join(marker).exists())
        {
            return Some(candidate.to_path_buf());
        }
    }

    if start.exists() {
        Some(start.to_path_buf())
    } else {
        None
    }
}

fn project_name_from_root(root: &Path) -> Option<String> {
    root.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
}

fn load_state() -> Result<StateFile> {
    let path = state_file_path()?;
    if !path.exists() {
        return Ok(StateFile {
            version: 1,
            updated_at_epoch: now_epoch(),
            ports: BTreeMap::new(),
        });
    }

    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut state: StateFile = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if state.version == 0 {
        state.version = 1;
    }
    Ok(state)
}

fn save_state(state: &StateFile) -> Result<()> {
    let path = state_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(state)?;
    fs::write(&path, json).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn state_file_path() -> Result<PathBuf> {
    let base =
        dirs::data_local_dir().ok_or_else(|| anyhow!("could not determine data directory"))?;
    let current = base.join("cli-cockpit").join("state.json");
    if current.exists() {
        return Ok(current);
    }

    let legacy = base.join("portledger").join("state.json");
    if legacy.exists() {
        return Ok(legacy);
    }

    Ok(current)
}

fn merge_state(state: &mut StateFile, active: &[PortSnapshot]) {
    let now = now_epoch();
    state.version = 1;
    state.updated_at_epoch = now;

    let mut active_ports = HashSet::new();
    for snapshot in active {
        active_ports.insert(snapshot.port);
        let owner = snapshot.owners.first();
        let entry = state
            .ports
            .entry(snapshot.port)
            .or_insert_with(|| PersistedPortRecord {
                port: snapshot.port,
                ..PersistedPortRecord::default()
            });

        if entry.first_seen_epoch == 0 {
            entry.first_seen_epoch = now;
        }
        entry.last_seen_epoch = now;
        entry.released_at_epoch = None;
        entry.last_status = PersistedStatus::Listening;
        entry.pid = owner.map(|item| item.pid);
        entry.project_name = owner.map(|item| item.project_name.clone());
        entry.project_root = owner.and_then(|item| item.project_root.clone());
        entry.command_line = owner.map(|item| item.command_line.clone());
    }

    for entry in state.ports.values_mut() {
        if !active_ports.contains(&entry.port) && entry.last_status == PersistedStatus::Listening {
            entry.last_status = PersistedStatus::Released;
            entry.released_at_epoch = Some(now);
        }
    }
}

fn recent_released_records(state: &StateFile, limit: usize) -> Vec<PersistedPortRecord> {
    let mut released: Vec<_> = state
        .ports
        .values()
        .filter(|entry| entry.last_status == PersistedStatus::Released)
        .cloned()
        .collect();
    released.sort_by_key(|entry| std::cmp::Reverse(entry.released_at_epoch.unwrap_or(0)));
    released.truncate(limit);
    released
}

fn free_ports(from: u16, to: u16, active: &[PortSnapshot], count: usize) -> Vec<u16> {
    let occupied: HashSet<u16> = active.iter().map(|entry| entry.port).collect();
    (from..=to)
        .filter(|port| !occupied.contains(port))
        .take(count)
        .collect()
}

impl DashboardState {
    fn new(snapshot: Snapshot) -> Self {
        let mut state = Self {
            snapshot,
            all_rows: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            message: DashboardMessage::info(
                "Use ? or F1 for command menu. Use Up/Down to inspect sessions. Ctrl+C quits.",
            ),
            filter: None,
            sort: DashboardSort::default(),
            health_cache: HashMap::new(),
        };
        state.rebuild_rows(None);
        state
    }

    fn selected_row(&self) -> Option<&DashboardRow> {
        self.rows.get(self.selected)
    }

    fn selected_key(&self) -> Option<(u16, i32)> {
        self.selected_row().map(DashboardRow::key)
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot) {
        self.snapshot = snapshot;
        self.health_cache.retain(|(port, pid), _| {
            self.snapshot.active.iter().any(|entry| {
                entry.port == *port && entry.owners.iter().any(|owner| owner.pid == *pid)
            })
        });
        self.rebuild_rows(None);
    }

    fn apply_health_sample(&mut self, port: u16, pid: i32, health: HealthStatus) {
        self.health_cache.insert((port, pid), health);
    }

    fn rebuild_rows(&mut self, preferred_key: Option<(u16, i32)>) {
        let selected_key = preferred_key.or_else(|| self.selected_key());
        self.all_rows = build_dashboard_rows(&self.snapshot);
        self.rows = self
            .all_rows
            .iter()
            .filter(|row| self.filter_matches(row))
            .cloned()
            .collect();
        self.sort_rows();
        self.selected = if self.rows.is_empty() {
            0
        } else if let Some((port, pid)) = selected_key {
            self.rows
                .iter()
                .position(|row| row.key() == (port, pid))
                .or_else(|| self.rows.iter().position(|row| row.port == port))
                .unwrap_or_else(|| self.selected.min(self.rows.len().saturating_sub(1)))
        } else {
            self.selected.min(self.rows.len().saturating_sub(1))
        };
    }

    fn filter_matches(&self, row: &DashboardRow) -> bool {
        match self.filter.as_ref() {
            None => true,
            Some(DashboardFilter::Stale) => row.owner.stale,
            Some(DashboardFilter::Active) => !row.owner.stale,
            Some(DashboardFilter::Health(filter)) => {
                self.health_for(row).class() == HealthClass::from(*filter)
            }
            Some(DashboardFilter::Port(port)) => row.port == *port,
            Some(DashboardFilter::ProjectContains(needle)) => {
                row.owner.project_name.to_ascii_lowercase().contains(needle)
            }
            Some(DashboardFilter::CommandContains(needle)) => {
                row.owner.command_line.to_ascii_lowercase().contains(needle)
            }
        }
    }

    fn health_for(&self, row: &DashboardRow) -> HealthStatus {
        self.health_cache
            .get(&row.key())
            .cloned()
            .unwrap_or(HealthStatus::Unknown)
    }

    fn sort_rows(&mut self) {
        let sort = self.sort;
        self.rows.sort_by(|left, right| {
            let base =
                compare_rows_by_sort(sort.key, sort.direction, left, right, &self.health_cache);
            if base == Ordering::Equal {
                left.port
                    .cmp(&right.port)
                    .then_with(|| left.owner.pid.cmp(&right.owner.pid))
            } else {
                base
            }
        });
    }

    fn filter_label(&self) -> String {
        self.filter
            .as_ref()
            .map(DashboardFilter::label)
            .unwrap_or_else(|| "none".to_string())
    }

    fn sort_label(&self) -> String {
        format!("{} {}", self.sort.key.label(), self.sort.direction.label())
    }
}

impl WorktreeDashboardState {
    fn new(repo: Option<RepoSnapshot>) -> Self {
        let mut state = Self {
            repo,
            rows: Vec::new(),
            all_rows: Vec::new(),
            selected: 0,
            filter: None,
            sort: WorktreeSort::default(),
            message: DashboardMessage::info(
                "Press Tab to switch views. In Worktrees, try `new feature-x`, `sync all --from main`, or `task test`.",
            ),
            last_task: None,
        };
        state.rebuild_rows(None);
        state
    }

    fn selected_row(&self) -> Option<&WorktreeRow> {
        self.rows.get(self.selected)
    }

    fn selected_key(&self) -> Option<String> {
        self.selected_row()
            .map(|row| row.worktree.path.display().to_string())
    }

    fn apply_repo(&mut self, repo: Option<RepoSnapshot>) {
        self.repo = repo;
        self.rebuild_rows(None);
    }

    fn rebuild_rows(&mut self, preferred_path: Option<&str>) {
        let selected_key = preferred_path
            .map(ToOwned::to_owned)
            .or_else(|| self.selected_key());
        self.all_rows = self
            .repo
            .as_ref()
            .map(build_worktree_rows)
            .unwrap_or_default();
        self.rows = self
            .all_rows
            .iter()
            .filter(|row| self.filter_matches(row))
            .cloned()
            .collect();
        self.sort_rows();
        self.selected = if self.rows.is_empty() {
            0
        } else if let Some(path) = selected_key {
            self.rows
                .iter()
                .position(|row| row.worktree.path == Path::new(&path))
                .unwrap_or_else(|| self.selected.min(self.rows.len().saturating_sub(1)))
        } else {
            self.selected.min(self.rows.len().saturating_sub(1))
        };
    }

    fn filter_matches(&self, row: &WorktreeRow) -> bool {
        match self.filter.as_ref() {
            None => true,
            Some(WorktreeFilter::Dirty) => row.worktree.is_dirty(),
            Some(WorktreeFilter::Clean) => !row.worktree.is_dirty(),
            Some(WorktreeFilter::Conflicted) => !row.worktree.conflicted.is_empty(),
            Some(WorktreeFilter::Locked) => row.worktree.locked,
            Some(WorktreeFilter::Prunable) => row.worktree.prunable,
            Some(WorktreeFilter::Detached) => row.worktree.detached,
            Some(WorktreeFilter::BranchContains(needle)) => row
                .worktree
                .branch_label()
                .to_ascii_lowercase()
                .contains(needle),
            Some(WorktreeFilter::PathContains(needle)) => row
                .worktree
                .path
                .display()
                .to_string()
                .to_ascii_lowercase()
                .contains(needle),
        }
    }

    fn sort_rows(&mut self) {
        let sort = self.sort;
        self.rows.sort_by(|left, right| {
            let base = compare_worktrees_by_sort(sort.key, sort.direction, left, right);
            if base == Ordering::Equal {
                left.worktree.path.cmp(&right.worktree.path)
            } else {
                base
            }
        });
    }

    fn filter_label(&self) -> String {
        self.filter
            .as_ref()
            .map(WorktreeFilter::label)
            .unwrap_or_else(|| "none".to_string())
    }

    fn sort_label(&self) -> String {
        format!("{} {}", self.sort.key.label(), self.sort.direction.label())
    }
}

impl CockpitState {
    fn new(ports: DashboardState, worktrees: WorktreeDashboardState) -> Self {
        Self {
            view: CockpitView::Ports,
            ports,
            worktrees,
            input: String::new(),
            show_command_menu: false,
            pending_confirmation: None,
        }
    }

    fn active_message(&self) -> &DashboardMessage {
        match self.view {
            CockpitView::Ports => &self.ports.message,
            CockpitView::Worktrees => &self.worktrees.message,
        }
    }

    fn active_message_mut(&mut self) -> &mut DashboardMessage {
        match self.view {
            CockpitView::Ports => &mut self.ports.message,
            CockpitView::Worktrees => &mut self.worktrees.message,
        }
    }

    fn move_selection_up(&mut self, amount: usize) {
        match self.view {
            CockpitView::Ports => {
                self.ports.selected = self.ports.selected.saturating_sub(amount);
            }
            CockpitView::Worktrees => {
                self.worktrees.selected = self.worktrees.selected.saturating_sub(amount);
            }
        }
    }

    fn move_selection_down(&mut self, amount: usize) {
        match self.view {
            CockpitView::Ports => {
                if !self.ports.rows.is_empty() {
                    self.ports.selected =
                        (self.ports.selected + amount).min(self.ports.rows.len().saturating_sub(1));
                }
            }
            CockpitView::Worktrees => {
                if !self.worktrees.rows.is_empty() {
                    self.worktrees.selected = (self.worktrees.selected + amount)
                        .min(self.worktrees.rows.len().saturating_sub(1));
                }
            }
        }
    }

    fn move_selection_home(&mut self) {
        match self.view {
            CockpitView::Ports => self.ports.selected = 0,
            CockpitView::Worktrees => self.worktrees.selected = 0,
        }
    }

    fn move_selection_end(&mut self) {
        match self.view {
            CockpitView::Ports => {
                if !self.ports.rows.is_empty() {
                    self.ports.selected = self.ports.rows.len().saturating_sub(1);
                }
            }
            CockpitView::Worktrees => {
                if !self.worktrees.rows.is_empty() {
                    self.worktrees.selected = self.worktrees.rows.len().saturating_sub(1);
                }
            }
        }
    }
}

impl CockpitView {
    fn label(&self) -> &'static str {
        match self {
            CockpitView::Ports => "ports",
            CockpitView::Worktrees => "worktrees",
        }
    }
}

impl DashboardRow {
    fn key(&self) -> (u16, i32) {
        (self.port, self.owner.pid)
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl WorktreeRow {
    fn key(&self) -> &Path {
        &self.worktree.path
    }
}

impl DashboardSort {
    fn label(&self) -> String {
        format!("{} {}", self.key.label(), self.direction.label())
    }
}

impl Default for DashboardSort {
    fn default() -> Self {
        Self {
            key: DashboardSortKey::Age,
            direction: SortDirection::Asc,
        }
    }
}

impl DashboardSortKey {
    fn label(&self) -> &'static str {
        match self {
            DashboardSortKey::Port => "port",
            DashboardSortKey::Project => "project",
            DashboardSortKey::Age => "age",
            DashboardSortKey::State => "state",
            DashboardSortKey::Health => "health",
        }
    }
}

impl WorktreeSort {
    fn label(&self) -> String {
        format!("{} {}", self.key.label(), self.direction.label())
    }
}

impl Default for WorktreeSort {
    fn default() -> Self {
        Self {
            key: WorktreeSortKey::State,
            direction: SortDirection::Desc,
        }
    }
}

impl WorktreeSortKey {
    fn label(&self) -> &'static str {
        match self {
            WorktreeSortKey::Path => "path",
            WorktreeSortKey::Branch => "branch",
            WorktreeSortKey::Sync => "sync",
            WorktreeSortKey::Changes => "changes",
            WorktreeSortKey::State => "state",
            WorktreeSortKey::Stale => "stale",
        }
    }
}

impl SortDirection {
    fn label(&self) -> &'static str {
        match self {
            SortDirection::Asc => "asc",
            SortDirection::Desc => "desc",
        }
    }
}

impl DashboardFilter {
    fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            bail!("usage: filter <expr>");
        }

        let normalized = trimmed.to_ascii_lowercase();
        match normalized.as_str() {
            "stale" => return Ok(Self::Stale),
            "active" => return Ok(Self::Active),
            "health:up" => return Ok(Self::Health(HealthFilter::Up)),
            "health:tcp" => return Ok(Self::Health(HealthFilter::Tcp)),
            "health:down" => return Ok(Self::Health(HealthFilter::Down)),
            "health:unknown" => return Ok(Self::Health(HealthFilter::Unknown)),
            _ => {}
        }

        if let Some(port) = normalized.strip_prefix("port:") {
            return Ok(Self::Port(parse_port_token(port)?));
        }
        if let Some(project) = normalized.strip_prefix("project:") {
            if project.is_empty() {
                bail!("usage: filter project:<substring>");
            }
            return Ok(Self::ProjectContains(project.to_string()));
        }
        if let Some(command) = normalized.strip_prefix("cmd:") {
            if command.is_empty() {
                bail!("usage: filter cmd:<substring>");
            }
            return Ok(Self::CommandContains(command.to_string()));
        }

        bail!("unknown filter `{}`", trimmed)
    }

    fn label(&self) -> String {
        match self {
            DashboardFilter::Stale => "stale".to_string(),
            DashboardFilter::Active => "active".to_string(),
            DashboardFilter::Health(value) => format!("health:{}", value.label()),
            DashboardFilter::Port(port) => format!("port:{}", port),
            DashboardFilter::ProjectContains(value) => format!("project:{}", value),
            DashboardFilter::CommandContains(value) => format!("cmd:{}", value),
        }
    }
}

impl WorktreeFilter {
    fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            bail!("usage: filter <expr>");
        }

        let normalized = trimmed.to_ascii_lowercase();
        match normalized.as_str() {
            "dirty" => return Ok(Self::Dirty),
            "clean" => return Ok(Self::Clean),
            "conflicted" => return Ok(Self::Conflicted),
            "locked" => return Ok(Self::Locked),
            "prunable" => return Ok(Self::Prunable),
            "detached" => return Ok(Self::Detached),
            _ => {}
        }

        if let Some(branch) = normalized.strip_prefix("branch:") {
            if branch.is_empty() {
                bail!("usage: filter branch:<substring>");
            }
            return Ok(Self::BranchContains(branch.to_string()));
        }
        if let Some(path) = normalized.strip_prefix("path:") {
            if path.is_empty() {
                bail!("usage: filter path:<substring>");
            }
            return Ok(Self::PathContains(path.to_string()));
        }

        bail!("unknown filter `{}`", trimmed)
    }

    fn label(&self) -> String {
        match self {
            WorktreeFilter::Dirty => "dirty".to_string(),
            WorktreeFilter::Clean => "clean".to_string(),
            WorktreeFilter::Conflicted => "conflicted".to_string(),
            WorktreeFilter::Locked => "locked".to_string(),
            WorktreeFilter::Prunable => "prunable".to_string(),
            WorktreeFilter::Detached => "detached".to_string(),
            WorktreeFilter::BranchContains(value) => format!("branch:{}", value),
            WorktreeFilter::PathContains(value) => format!("path:{}", value),
        }
    }
}

impl WorktreeRecord {
    fn branch_label(&self) -> String {
        self.branch
            .clone()
            .unwrap_or_else(|| "detached".to_string())
    }

    fn kind_label(&self) -> &'static str {
        if self.is_main {
            "main"
        } else if self.detached {
            "detached"
        } else {
            "linked"
        }
    }

    fn change_count(&self) -> usize {
        self.staged.len() + self.unstaged.len() + self.untracked.len() + self.conflicted.len()
    }

    fn is_dirty(&self) -> bool {
        self.change_count() > 0
    }

    fn sync_score(&self) -> (u32, u32, u32) {
        (self.ahead + self.behind, self.behind, self.ahead)
    }

    fn unpushed_count(&self) -> Option<u32> {
        if self.upstream.is_some() {
            Some(self.ahead)
        } else if self.origin_ref.is_some() {
            Some(self.origin_ahead)
        } else {
            None
        }
    }

    fn has_in_progress_operation(&self) -> bool {
        !self.operations.is_empty()
    }

    fn stale_score(&self, now_epoch: i64) -> u32 {
        let mut score = 0u32;
        let commit_days = self
            .last_commit
            .as_ref()
            .and_then(|commit| {
                if commit.committed_at_epoch > 0 && commit.committed_at_epoch <= now_epoch {
                    Some(((now_epoch - commit.committed_at_epoch) as u64 / 86_400).min(365))
                } else {
                    None
                }
            })
            .unwrap_or(0);
        score = score.saturating_add(commit_days as u32);

        let checkout_days = self
            .last_checkout_epoch
            .and_then(|epoch| {
                if epoch > 0 && epoch <= now_epoch {
                    Some(((now_epoch - epoch) as u64 / 86_400).min(365))
                } else {
                    None
                }
            })
            .unwrap_or(0);
        score = score.saturating_add((checkout_days as u32).saturating_mul(2));

        if !self.is_dirty() {
            score = score.saturating_add(40);
        } else {
            score = score.saturating_sub(20);
        }

        match self.merged_into_base {
            Some(true) => score = score.saturating_add(80),
            Some(false) => score = score.saturating_sub(15),
            None => {}
        }

        match self.unpushed_count() {
            Some(0) => score = score.saturating_add(10),
            Some(_) => score = score.saturating_sub(20),
            None => {}
        }

        if self.has_in_progress_operation() {
            score = score.saturating_sub(80);
        }
        if self.locked {
            score = score.saturating_sub(30);
        }
        if self.is_main {
            score = score.saturating_sub(120);
        }

        score
    }

    fn state_rank(&self) -> u8 {
        if !self.conflicted.is_empty() {
            5
        } else if !self.operations.is_empty() {
            4
        } else if self.prunable {
            3
        } else if self.locked {
            2
        } else if self.is_dirty() {
            1
        } else {
            0
        }
    }

    fn flag_summary(&self) -> String {
        let mut flags = Vec::new();
        if self.is_dirty() {
            flags.push("dirty".to_string());
        } else {
            flags.push("clean".to_string());
        }
        if self.locked {
            flags.push("locked".to_string());
        }
        if self.prunable {
            flags.push("prunable".to_string());
        }
        if self.unpushed_count().unwrap_or(0) > 0 {
            flags.push(format!("unpushed:{}", self.unpushed_count().unwrap_or(0)));
        }
        match self.merged_into_base {
            Some(true) => flags.push("merged".to_string()),
            Some(false) => flags.push("unmerged".to_string()),
            None => {}
        }
        for operation in &self.operations {
            flags.push(operation.label().to_string());
        }
        flags.join(",")
    }
}

fn build_worktree_rows(repo: &RepoSnapshot) -> Vec<WorktreeRow> {
    repo.worktrees
        .iter()
        .cloned()
        .map(|worktree| WorktreeRow { worktree })
        .collect()
}

fn compare_worktrees_by_sort(
    key: WorktreeSortKey,
    direction: SortDirection,
    left: &WorktreeRow,
    right: &WorktreeRow,
) -> Ordering {
    let now_epoch = now_epoch();
    let ordering = match key {
        WorktreeSortKey::Path => left.worktree.path.cmp(&right.worktree.path),
        WorktreeSortKey::Branch => left
            .worktree
            .branch_label()
            .to_ascii_lowercase()
            .cmp(&right.worktree.branch_label().to_ascii_lowercase()),
        WorktreeSortKey::Sync => left.worktree.sync_score().cmp(&right.worktree.sync_score()),
        WorktreeSortKey::Changes => left
            .worktree
            .change_count()
            .cmp(&right.worktree.change_count()),
        WorktreeSortKey::State => left.worktree.state_rank().cmp(&right.worktree.state_rank()),
        WorktreeSortKey::Stale => left
            .worktree
            .stale_score(now_epoch)
            .cmp(&right.worktree.stale_score(now_epoch)),
    };

    match direction {
        SortDirection::Asc => ordering,
        SortDirection::Desc => ordering.reverse(),
    }
}

impl HealthFilter {
    fn label(&self) -> &'static str {
        match self {
            HealthFilter::Up => "up",
            HealthFilter::Tcp => "tcp",
            HealthFilter::Down => "down",
            HealthFilter::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthClass {
    Up,
    Tcp,
    Down,
    Unknown,
}

impl From<HealthFilter> for HealthClass {
    fn from(value: HealthFilter) -> Self {
        match value {
            HealthFilter::Up => HealthClass::Up,
            HealthFilter::Tcp => HealthClass::Tcp,
            HealthFilter::Down => HealthClass::Down,
            HealthFilter::Unknown => HealthClass::Unknown,
        }
    }
}

impl HealthStatus {
    fn class(&self) -> HealthClass {
        match self {
            HealthStatus::Up(_) => HealthClass::Up,
            HealthStatus::Tcp => HealthClass::Tcp,
            HealthStatus::Down(_) => HealthClass::Down,
            HealthStatus::Unknown => HealthClass::Unknown,
        }
    }

    fn badge(&self) -> String {
        match self {
            HealthStatus::Up(code) => format!("up:{code}"),
            HealthStatus::Tcp => "tcp".to_string(),
            HealthStatus::Down(_) => "down".to_string(),
            HealthStatus::Unknown => "?".to_string(),
        }
    }

    fn details(&self) -> String {
        match self {
            HealthStatus::Up(code) => format!("HTTP {}", code),
            HealthStatus::Tcp => "TCP open, no HTTP response".to_string(),
            HealthStatus::Down(reason) => format!("down ({})", reason),
            HealthStatus::Unknown => "not probed yet".to_string(),
        }
    }

    fn rank(&self) -> u8 {
        match self {
            HealthStatus::Up(_) => 0,
            HealthStatus::Tcp => 1,
            HealthStatus::Unknown => 2,
            HealthStatus::Down(_) => 3,
        }
    }
}

impl DashboardMessage {
    fn info(text: impl Into<String>) -> Self {
        Self {
            level: MessageLevel::Info,
            text: text.into(),
        }
    }

    fn success(text: impl Into<String>) -> Self {
        Self {
            level: MessageLevel::Success,
            text: text.into(),
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            level: MessageLevel::Error,
            text: text.into(),
        }
    }

    fn render(&self) -> String {
        let label = match self.level {
            MessageLevel::Info => "info",
            MessageLevel::Success => "ok",
            MessageLevel::Error => "error",
        };
        format!("[{label}] {}", self.text)
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter(stdout: &mut io::Stdout) -> Result<Self> {
        enable_raw_mode().context("failed to enable raw terminal mode")?;
        execute!(stdout, EnterAlternateScreen, Clear(ClearType::All), Show)
            .context("failed to enter dashboard mode")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen, Clear(ClearType::All));
    }
}

fn run_dashboard(args: MapArgs) -> Result<ExitCode> {
    let snapshot = refresh_snapshot(args.stale_after)?;
    let mut ports = DashboardState::new(snapshot);
    probe_visible_rows(&mut ports, Duration::from_millis(180))?;
    ports.rebuild_rows(None);
    let worktrees = WorktreeDashboardState::new(discover_current_repo()?);
    let mut state = CockpitState::new(ports, worktrees);
    let mut stdout = io::stdout();
    let _guard = TerminalGuard::enter(&mut stdout)?;
    let mut last_refresh = Instant::now();

    loop {
        render_dashboard(&mut stdout, &state, &args)?;

        if event::poll(Duration::from_millis(250)).context("failed to poll terminal input")? {
            let event = event::read().context("failed to read terminal input")?;
            if handle_dashboard_event(&mut state, event, &args)? {
                break;
            }
        }

        if last_refresh.elapsed() >= args.refresh_every {
            if let Err(error) = refresh_active_view(&mut state, args.stale_after) {
                *state.active_message_mut() =
                    DashboardMessage::error(format!("refresh failed: {error:#}"));
            }
            last_refresh = Instant::now();
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn handle_dashboard_event(state: &mut CockpitState, event: Event, args: &MapArgs) -> Result<bool> {
    let Event::Key(key) = event else {
        return Ok(false);
    };

    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(false);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => return Ok(true),
            KeyCode::Char('u') => {
                state.input.clear();
                return Ok(false);
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Tab => {
            switch_view(state, next_view(state.view), args.stale_after)?;
        }
        KeyCode::BackTab => {
            switch_view(state, previous_view(state.view), args.stale_after)?;
        }
        KeyCode::F(1) => {
            state.show_command_menu = !state.show_command_menu;
            *state.active_message_mut() = DashboardMessage::info(if state.show_command_menu {
                "Opened command menu."
            } else {
                "Closed command menu."
            });
        }
        KeyCode::Up => {
            state.move_selection_up(1);
        }
        KeyCode::Down => {
            state.move_selection_down(1);
        }
        KeyCode::PageUp => {
            state.move_selection_up(5);
        }
        KeyCode::PageDown => {
            state.move_selection_down(5);
        }
        KeyCode::Home => {
            state.move_selection_home();
        }
        KeyCode::End => {
            state.move_selection_end();
        }
        KeyCode::Esc => {
            if state.pending_confirmation.take().is_some() {
                *state.active_message_mut() = DashboardMessage::info("Canceled pending action.");
            } else if state.show_command_menu {
                state.show_command_menu = false;
                *state.active_message_mut() = DashboardMessage::info("Closed command menu.");
            } else {
                state.input.clear();
                *state.active_message_mut() = DashboardMessage::info("Cleared command input.");
            }
        }
        KeyCode::Backspace => {
            state.input.pop();
        }
        KeyCode::Enter => {
            let input = std::mem::take(&mut state.input);
            if input.trim().is_empty() {
                *state.active_message_mut() =
                    DashboardMessage::info("Enter a command or use Up/Down to inspect rows.");
                return Ok(false);
            }

            match execute_cockpit_command(&input, state, args) {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(error) => {
                    *state.active_message_mut() = DashboardMessage::error(format!("{error:#}"));
                }
            }
        }
        KeyCode::Char('?') if state.input.is_empty() => {
            state.show_command_menu = !state.show_command_menu;
            *state.active_message_mut() = DashboardMessage::info(if state.show_command_menu {
                "Opened command menu."
            } else {
                "Closed command menu."
            });
        }
        KeyCode::Char(ch)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            state.input.push(ch);
        }
        _ => {}
    }

    Ok(false)
}

fn execute_cockpit_command(input: &str, state: &mut CockpitState, args: &MapArgs) -> Result<bool> {
    let trimmed = input.trim();
    let normalized = trimmed.to_ascii_lowercase();

    if state.pending_confirmation.is_some() {
        return match normalized.as_str() {
            "yes" | "y" => {
                execute_pending_confirmation(state)?;
                Ok(false)
            }
            "no" | "n" => {
                state.pending_confirmation = None;
                *state.active_message_mut() = DashboardMessage::info("Canceled pending action.");
                Ok(false)
            }
            "quit" | "exit" | "q" => Ok(true),
            _ => {
                *state.active_message_mut() =
                    DashboardMessage::info("Type `yes` or `no` to resolve the pending action.");
                Ok(false)
            }
        };
    }

    if trimmed.is_empty() {
        refresh_active_view(state, args.stale_after)?;
        return Ok(false);
    }

    if normalized == "help"
        || normalized == "?"
        || normalized.starts_with("help ")
        || normalized.starts_with("? ")
        || matches!(normalized.as_str(), "commands" | "command" | "menu")
    {
        state.show_command_menu = true;
        *state.active_message_mut() =
            DashboardMessage::info(build_help_message(state.view, parse_help_topic(trimmed)));
        return Ok(false);
    }

    if matches!(normalized.as_str(), "quit" | "exit" | "q") {
        return Ok(true);
    }

    if matches!(normalized.as_str(), "refresh" | "r") {
        refresh_active_view(state, args.stale_after)?;
        return Ok(false);
    }

    if matches!(normalized.as_str(), "yes" | "y" | "no" | "n") {
        *state.active_message_mut() =
            DashboardMessage::info("No action is waiting for confirmation.");
        return Ok(false);
    }

    if matches!(normalized.as_str(), "ports" | "port") {
        switch_view(state, CockpitView::Ports, args.stale_after)?;
        return Ok(false);
    }

    if matches!(normalized.as_str(), "worktrees" | "worktree" | "git") {
        switch_view(state, CockpitView::Worktrees, args.stale_after)?;
        return Ok(false);
    }

    if let Some(view_name) = normalized.strip_prefix("view ") {
        let view = match view_name.trim() {
            "ports" | "port" => CockpitView::Ports,
            "worktrees" | "worktree" | "git" => CockpitView::Worktrees,
            other => bail!("unknown view `{other}`"),
        };
        switch_view(state, view, args.stale_after)?;
        return Ok(false);
    }

    if normalized.starts_with("download")
        || normalized.starts_with("export")
        || normalized == "d"
        || normalized.starts_with("d ")
    {
        let path = parse_download_target(trimmed)?;
        let snapshot = refresh_snapshot(args.stale_after)?;
        let repo = discover_current_repo()?;
        let export_path = resolve_export_path(path.as_deref())?;
        export_dashboard_snapshot(&snapshot, repo.as_ref(), args.stale_after, &export_path)?;
        state.ports.apply_snapshot(snapshot);
        state.worktrees.apply_repo(repo);
        *state.active_message_mut() =
            DashboardMessage::success(format!("Wrote {}", export_path.display()));
        return Ok(false);
    }

    if let Some(rest) = strip_worktree_command_prefix(trimmed) {
        execute_worktree_command(rest.trim(), state)?;
        return Ok(false);
    }

    if matches!(state.view, CockpitView::Worktrees) && is_bare_worktree_command(trimmed) {
        execute_worktree_command(trimmed, state)?;
        return Ok(false);
    }

    match state.view {
        CockpitView::Ports => execute_dashboard_command(trimmed, &mut state.ports, args),
        CockpitView::Worktrees => execute_worktree_view_command(trimmed, state),
    }
}

fn parse_help_topic(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    if trimmed == "help"
        || trimmed == "?"
        || trimmed == "commands"
        || trimmed == "command"
        || trimmed == "menu"
    {
        return None;
    }

    trimmed
        .strip_prefix("help ")
        .or_else(|| trimmed.strip_prefix("? "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn build_help_message(view: CockpitView, topic: Option<&str>) -> String {
    match topic.map(|value| value.to_ascii_lowercase()) {
        Some(topic) if matches!(topic.as_str(), "ports" | "port") => {
            "Ports: restart/kill/move/open/select/filter/sort/clear. Use `ports` to jump here."
                .to_string()
        }
        Some(topic) if matches!(topic.as_str(), "worktrees" | "worktree" | "git") => {
            "Worktrees: use bare verbs like `new`, `switch`, `remove`, `sync all`, `task test`, or explicit prefixes `wt` / `worktree`."
                .to_string()
        }
        Some(topic) if matches!(topic.as_str(), "commands" | "command" | "menu") => {
            "Opened command menu. Use `ports`, `worktrees`, `download`, `yes`, and `no` as top-level commands."
                .to_string()
        }
        Some(other) => format!("No dedicated help topic for `{other}`. Use `help ports`, `help worktrees`, or `commands`."),
        None => match view {
            CockpitView::Ports => {
                "Ports view: use `ports`, `worktrees`, `download`, and port commands like `restart`, `kill`, and `move`."
                    .to_string()
            }
            CockpitView::Worktrees => {
                "Worktrees view: bare verbs work here, so `new feature-x`, `switch main`, `sync all --from main`, and `task test` are valid."
                    .to_string()
            }
        },
    }
}

fn strip_worktree_command_prefix(input: &str) -> Option<&str> {
    ["wt", "worktree", "worktrees"]
        .into_iter()
        .find_map(|prefix| {
            input.strip_prefix(prefix).and_then(|rest| {
                let trimmed = rest.trim_start();
                if trimmed.len() == rest.len() {
                    None
                } else {
                    Some(trimmed)
                }
            })
        })
        .filter(|rest| !rest.is_empty())
}

fn is_bare_worktree_command(input: &str) -> bool {
    let Some(command) = input.split_whitespace().next() else {
        return false;
    };

    matches!(
        command.to_ascii_lowercase().as_str(),
        "add"
            | "create"
            | "new"
            | "switch"
            | "checkout"
            | "co"
            | "move"
            | "mv"
            | "lock"
            | "unlock"
            | "remove"
            | "delete"
            | "rm"
            | "prune"
            | "cleanup"
            | "cherry-pick"
            | "pick"
            | "merge"
            | "rebase"
            | "reset"
            | "continue"
            | "cont"
            | "abort"
            | "sync"
            | "task"
            | "test"
            | "lint"
            | "build"
    )
}

fn execute_pending_confirmation(state: &mut CockpitState) -> Result<()> {
    let Some(pending) = state.pending_confirmation.take() else {
        *state.active_message_mut() =
            DashboardMessage::info("No action is waiting for confirmation.");
        return Ok(());
    };

    match pending.action {
        ConfirmableAction::RemoveWorktree { target, force } => {
            let request = WorktreeRemoveRequest {
                target: Some(target),
                force,
            };
            perform_remove_worktree(state, &request)?;
        }
        ConfirmableAction::PruneWorktrees { dry_run } => {
            perform_prune_worktrees(state, dry_run)?;
        }
        ConfirmableAction::CherryPickWorktree { target, commits } => {
            let request = WorktreeCherryPickRequest {
                commits,
                target: Some(target),
            };
            perform_cherry_pick_worktree(state, &request)?;
        }
        ConfirmableAction::MergeWorktree { target, reference } => {
            let request = WorktreeRefRequest {
                reference,
                target: Some(target),
            };
            perform_merge_worktree(state, &request)?;
        }
        ConfirmableAction::RebaseWorktree { target, reference } => {
            let request = WorktreeRefRequest {
                reference,
                target: Some(target),
            };
            perform_rebase_worktree(state, &request)?;
        }
        ConfirmableAction::ResetWorktree {
            target,
            mode,
            reference,
        } => {
            let request = WorktreeResetRequest {
                mode,
                reference,
                target: Some(target),
            };
            perform_reset_worktree(state, &request)?;
        }
    }

    Ok(())
}

fn refresh_active_view(state: &mut CockpitState, stale_after: Duration) -> Result<()> {
    match state.view {
        CockpitView::Ports => refresh_ports_view(state, stale_after),
        CockpitView::Worktrees => refresh_worktrees_view(state),
    }
}

fn refresh_ports_view(state: &mut CockpitState, stale_after: Duration) -> Result<()> {
    let snapshot = refresh_snapshot(stale_after)?;
    state.ports.apply_snapshot(snapshot);
    probe_visible_rows(&mut state.ports, Duration::from_millis(180))?;
    state.ports.rebuild_rows(None);
    Ok(())
}

fn refresh_worktrees_view(state: &mut CockpitState) -> Result<()> {
    let preferred = state
        .worktrees
        .selected_row()
        .map(|row| row.worktree.path.display().to_string());
    state.worktrees.apply_repo(discover_current_repo()?);
    state.worktrees.rebuild_rows(preferred.as_deref());
    Ok(())
}

fn switch_view(state: &mut CockpitState, view: CockpitView, stale_after: Duration) -> Result<()> {
    if state.view == view {
        return Ok(());
    }
    state.view = view;
    match view {
        CockpitView::Ports => {
            probe_visible_rows(&mut state.ports, Duration::from_millis(180))?;
            state.ports.rebuild_rows(None);
            state.ports.message = DashboardMessage::info("Switched to Ports view.");
        }
        CockpitView::Worktrees => {
            refresh_worktrees_view(state)?;
            state.worktrees.message = DashboardMessage::info("Switched to Worktrees view.");
        }
    }
    if matches!(view, CockpitView::Ports) {
        let _ = stale_after;
    }
    Ok(())
}

fn next_view(view: CockpitView) -> CockpitView {
    match view {
        CockpitView::Ports => CockpitView::Worktrees,
        CockpitView::Worktrees => CockpitView::Ports,
    }
}

fn previous_view(view: CockpitView) -> CockpitView {
    next_view(view)
}

fn parse_download_target(input: &str) -> Result<Option<String>> {
    let mut parts = input.split_whitespace();
    let Some(command) = parts.next() else {
        return Ok(None);
    };
    if !matches!(command, "download" | "export" | "d") {
        bail!("usage: download [file]");
    }
    Ok(parts.next().map(str::to_string))
}

fn execute_dashboard_command(
    input: &str,
    state: &mut DashboardState,
    args: &MapArgs,
) -> Result<bool> {
    match parse_dashboard_command(input)? {
        DashboardCommand::Help => {
            state.message = DashboardMessage::info(
                "Use ? or F1 for full command menu. Quick examples: quick stale, restart 3000 --port 3010, filter health:up, sort new-old, open 3000.",
            );
        }
        DashboardCommand::Refresh => {
            let snapshot = refresh_snapshot(args.stale_after)?;
            state.apply_snapshot(snapshot);
            state.message = DashboardMessage::success("Refreshed dashboard state.");
        }
        DashboardCommand::Kill(target) => {
            execute_kill_command(&target, state, args.stale_after)?;
        }
        DashboardCommand::Restart(request) => {
            execute_restart_command(&request, state, args.stale_after)?;
        }
        DashboardCommand::Quick(action) => {
            execute_quick_action(action, state, args.stale_after)?;
        }
        DashboardCommand::Move { from, to } => {
            execute_move_command(from, to, state, args.stale_after)?;
        }
        DashboardCommand::Download(path) => {
            let snapshot = refresh_snapshot(args.stale_after)?;
            let path = resolve_export_path(path.as_deref())?;
            let repo = discover_current_repo()?;
            export_dashboard_snapshot(&snapshot, repo.as_ref(), args.stale_after, &path)?;
            state.apply_snapshot(snapshot);
            state.message = DashboardMessage::success(format!("Wrote {}", path.display()));
        }
        DashboardCommand::Filter(expression) => {
            let parsed = DashboardFilter::parse(&expression)?;
            if matches!(parsed, DashboardFilter::Health(_)) {
                let keys: Vec<_> = state.rows.iter().map(DashboardRow::key).collect();
                probe_rows_by_keys(state, &keys, Duration::from_millis(180));
            }
            state.filter = Some(parsed.clone());
            state.rebuild_rows(None);
            state.message =
                DashboardMessage::success(format!("Filter set to `{}`.", parsed.label()));
        }
        DashboardCommand::Sort(sort) => {
            state.sort = sort;
            state.rebuild_rows(None);
            state.message = DashboardMessage::success(format!("Sort set to {}.", sort.label()));
        }
        DashboardCommand::Clear => {
            state.filter = None;
            state.sort = DashboardSort::default();
            state.rebuild_rows(None);
            state.message = DashboardMessage::success("Cleared filter and sort.");
        }
        DashboardCommand::Open(target) => {
            execute_open_command(target.as_deref(), state)?;
        }
        DashboardCommand::Select(target) => {
            execute_select_command(&target, state)?;
        }
        DashboardCommand::Quit => return Ok(true),
    }

    if let Err(error) = probe_visible_rows(state, Duration::from_millis(180)) {
        state.message = DashboardMessage::error(format!("health probe failed: {error:#}"));
    } else {
        state.rebuild_rows(None);
    }

    Ok(false)
}

fn parse_dashboard_command(input: &str) -> Result<DashboardCommand> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(DashboardCommand::Refresh);
    }

    let mut parts = trimmed.split_whitespace();
    let Some(command) = parts.next() else {
        return Ok(DashboardCommand::Refresh);
    };
    let rest = trimmed
        .find(char::is_whitespace)
        .map(|index| trimmed[index..].trim())
        .unwrap_or("");

    match command.to_ascii_lowercase().as_str() {
        "help" | "?" => Ok(DashboardCommand::Help),
        "refresh" | "r" => Ok(DashboardCommand::Refresh),
        "kill" | "release" | "k" => Ok(DashboardCommand::Kill(parts.map(str::to_string).collect())),
        "restart" | "rs" => parse_restart_command(rest),
        "quick" | "qa" => parse_quick_command(rest),
        "move" | "mv" | "m" => {
            let rest: Vec<_> = parts.collect();
            match rest.as_slice() {
                [to] => Ok(DashboardCommand::Move {
                    from: None,
                    to: parse_port_token(to)?,
                }),
                [from, to] => Ok(DashboardCommand::Move {
                    from: Some(parse_port_token(from)?),
                    to: parse_port_token(to)?,
                }),
                _ => bail!("usage: move <new-port> or move <from-port> <new-port>"),
            }
        }
        "download" | "export" | "d" => {
            Ok(DashboardCommand::Download(parts.next().map(str::to_string)))
        }
        "filter" | "f" => {
            if rest.is_empty() {
                bail!("usage: filter <expr>");
            }
            Ok(DashboardCommand::Filter(rest.to_string()))
        }
        "sort" => parse_sort_command(rest),
        "clear" => Ok(DashboardCommand::Clear),
        "open" | "browse" => Ok(DashboardCommand::Open(parts.next().map(str::to_string))),
        "select" | "s" => {
            let target = parts
                .next()
                .ok_or_else(|| anyhow!("usage: select <port-or-pid>"))?;
            Ok(DashboardCommand::Select(target.to_string()))
        }
        "quit" | "exit" | "q" => Ok(DashboardCommand::Quit),
        other => bail!("unknown command `{other}`. try `help`"),
    }
}

fn parse_restart_command(rest: &str) -> Result<DashboardCommand> {
    let tokens: Vec<_> = rest.split_whitespace().collect();
    let mut target = None;
    let mut port_override = None;
    let mut index = 0usize;

    while index < tokens.len() {
        match tokens[index] {
            "--port" => {
                let value = tokens
                    .get(index + 1)
                    .ok_or_else(|| anyhow!("usage: restart [<port|pid>] [--port <new-port>]"))?;
                port_override = Some(parse_port_token(value)?);
                index += 2;
            }
            token => {
                if target.is_some() {
                    bail!("usage: restart [<port|pid>] [--port <new-port>]");
                }
                target = Some(token.to_string());
                index += 1;
            }
        }
    }

    Ok(DashboardCommand::Restart(RestartRequest {
        target,
        port_override,
    }))
}

fn parse_sort_command(rest: &str) -> Result<DashboardCommand> {
    let tokens: Vec<_> = rest.split_whitespace().collect();
    let Some(key_token) = tokens.first() else {
        bail!("usage: sort <port|project|age|state|health> [asc|desc] | sort <new-old|old-new>");
    };

    match key_token.to_ascii_lowercase().as_str() {
        "new-old" | "newest-oldest" => {
            return Ok(DashboardCommand::Sort(DashboardSort {
                key: DashboardSortKey::Age,
                direction: SortDirection::Asc,
            }));
        }
        "old-new" | "oldest-newest" => {
            return Ok(DashboardCommand::Sort(DashboardSort {
                key: DashboardSortKey::Age,
                direction: SortDirection::Desc,
            }));
        }
        _ => {}
    }

    let key = match key_token.to_ascii_lowercase().as_str() {
        "port" => DashboardSortKey::Port,
        "project" => DashboardSortKey::Project,
        "age" => DashboardSortKey::Age,
        "state" => DashboardSortKey::State,
        "health" => DashboardSortKey::Health,
        _ => bail!("unknown sort key `{}`", key_token),
    };

    let direction = match tokens.get(1).map(|value| value.to_ascii_lowercase()) {
        None => SortDirection::Asc,
        Some(value) if value == "asc" => SortDirection::Asc,
        Some(value) if value == "desc" => SortDirection::Desc,
        Some(value) => bail!("unknown sort direction `{}`", value),
    };

    Ok(DashboardCommand::Sort(DashboardSort { key, direction }))
}

fn parse_quick_command(rest: &str) -> Result<DashboardCommand> {
    match rest.trim().to_ascii_lowercase().as_str() {
        "stale" | "kill-stale" => Ok(DashboardCommand::Quick(QuickAction::KillStale)),
        "old" | "kill-old" => Ok(DashboardCommand::Quick(QuickAction::KillOld)),
        "restart-old" => Ok(DashboardCommand::Quick(QuickAction::RestartOld)),
        _ => bail!("usage: quick <stale|old|restart-old>"),
    }
}

fn execute_worktree_view_command(input: &str, state: &mut CockpitState) -> Result<bool> {
    let trimmed = input.trim();
    let mut parts = trimmed.split_whitespace();
    let Some(command) = parts.next() else {
        return Ok(false);
    };
    let rest = trimmed
        .find(char::is_whitespace)
        .map(|index| trimmed[index..].trim())
        .unwrap_or("");

    match command.to_ascii_lowercase().as_str() {
        "filter" | "f" => {
            if rest.is_empty() {
                bail!("usage: filter <expr>");
            }
            let parsed = WorktreeFilter::parse(rest)?;
            state.worktrees.filter = Some(parsed.clone());
            state.worktrees.rebuild_rows(None);
            *state.active_message_mut() =
                DashboardMessage::success(format!("Filter set to `{}`.", parsed.label()));
        }
        "sort" => {
            let sort = parse_worktree_sort(rest)?;
            state.worktrees.sort = sort;
            state.worktrees.rebuild_rows(None);
            *state.active_message_mut() =
                DashboardMessage::success(format!("Sort set to {}.", sort.label()));
        }
        "clear" => {
            state.worktrees.filter = None;
            state.worktrees.sort = WorktreeSort::default();
            state.worktrees.rebuild_rows(None);
            *state.active_message_mut() = DashboardMessage::success("Cleared filter and sort.");
        }
        "select" | "s" => {
            let target = parts
                .next()
                .ok_or_else(|| anyhow!("usage: select <path-or-branch>"))?;
            execute_select_worktree_command(target, state)?;
        }
        "open" | "browse" => {
            execute_open_worktree_command(parts.next(), state)?;
        }
        other => bail!(
            "unknown worktree command `{other}`. try bare verbs like `add`/`remove`, `worktree ...`, or `help worktrees`"
        ),
    }

    Ok(false)
}

fn execute_worktree_command(input: &str, state: &mut CockpitState) -> Result<()> {
    match parse_worktree_command(input, &state.worktrees)? {
        WorktreeCommand::Add(request) => perform_add_worktree(state, &request),
        WorktreeCommand::Open(target) => execute_open_worktree_command(target.as_deref(), state),
        WorktreeCommand::Switch(request) => perform_switch_worktree(state, &request),
        WorktreeCommand::Move(request) => perform_move_worktree(state, &request),
        WorktreeCommand::Lock(request) => perform_lock_worktree(state, &request),
        WorktreeCommand::Unlock(target) => perform_unlock_worktree(state, target.as_deref()),
        WorktreeCommand::Remove(request) => queue_remove_worktree(state, &request),
        WorktreeCommand::Prune { dry_run } => {
            state.pending_confirmation = Some(PendingConfirmation {
                prompt: if dry_run {
                    "Run `git worktree prune --dry-run`?".to_string()
                } else {
                    "Prune stale git worktree metadata?".to_string()
                },
                action: ConfirmableAction::PruneWorktrees { dry_run },
            });
            *state.active_message_mut() =
                DashboardMessage::info("Pending confirmation. Type `yes` or `no`.");
            Ok(())
        }
        WorktreeCommand::CherryPick(request) => queue_cherry_pick_worktree(state, &request),
        WorktreeCommand::Merge(request) => queue_merge_worktree(state, &request),
        WorktreeCommand::Rebase(request) => queue_rebase_worktree(state, &request),
        WorktreeCommand::Reset(request) => queue_reset_worktree(state, &request),
        WorktreeCommand::Continue(target) => perform_continue_worktree(state, target.as_deref()),
        WorktreeCommand::Abort(target) => perform_abort_worktree(state, target.as_deref()),
        WorktreeCommand::Sync(request) => perform_sync_worktrees(state, &request),
        WorktreeCommand::Task(request) => perform_worktree_task(state, &request),
    }
}

fn parse_worktree_command(input: &str, state: &WorktreeDashboardState) -> Result<WorktreeCommand> {
    let tokens: Vec<String> = input.split_whitespace().map(ToOwned::to_owned).collect();
    let Some(command) = tokens.first().map(|token| token.to_ascii_lowercase()) else {
        bail!("usage: wt <action> ...");
    };

    match command.as_str() {
        "add" | "create" => parse_worktree_add(&tokens[1..], false),
        "new" => parse_worktree_add(&tokens[1..], true),
        "open" => Ok(WorktreeCommand::Open(tokens.get(1).cloned())),
        "switch" | "checkout" | "co" => parse_worktree_switch(&tokens[1..]),
        "move" | "mv" => parse_worktree_move(&tokens[1..]),
        "lock" => parse_worktree_lock(&tokens[1..]),
        "unlock" => Ok(WorktreeCommand::Unlock(tokens.get(1).cloned())),
        "remove" | "delete" | "rm" => parse_worktree_remove(&tokens[1..]),
        "prune" | "cleanup" => parse_worktree_prune(&tokens[1..]),
        "cherry-pick" | "pick" => parse_worktree_cherry_pick(&tokens[1..], state),
        "merge" => parse_worktree_ref_command("merge", &tokens[1..]).map(WorktreeCommand::Merge),
        "rebase" => parse_worktree_ref_command("rebase", &tokens[1..]).map(WorktreeCommand::Rebase),
        "reset" => parse_worktree_reset(&tokens[1..]),
        "continue" | "cont" => Ok(WorktreeCommand::Continue(tokens.get(1).cloned())),
        "abort" => Ok(WorktreeCommand::Abort(tokens.get(1).cloned())),
        "sync" => parse_worktree_sync(&tokens[1..]),
        "task" => parse_worktree_task(&tokens[1..]),
        "test" => parse_worktree_task_alias(WorktreeTaskPreset::Test, &tokens[1..]),
        "lint" => parse_worktree_task_alias(WorktreeTaskPreset::Lint, &tokens[1..]),
        "build" => parse_worktree_task_alias(WorktreeTaskPreset::Build, &tokens[1..]),
        other => bail!("unknown worktree action `{other}`"),
    }
}

fn parse_worktree_add(tokens: &[String], derive_from_name: bool) -> Result<WorktreeCommand> {
    let Some(name_or_path) = tokens.first() else {
        bail!(
            "usage: add <path> [--branch <name>] [--from <ref>] [--detach] [--no-checkout] [--lock]"
        );
    };

    let mut request = WorktreeAddRequest {
        path: name_or_path.clone(),
        branch: None,
        from_ref: None,
        detach: false,
        no_checkout: false,
        lock: false,
        implicit_path_from_branch: false,
    };
    let mut index = 1usize;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "--branch" => {
                request.branch = Some(
                    tokens
                        .get(index + 1)
                        .ok_or_else(|| anyhow!("missing value for --branch"))?
                        .clone(),
                );
                index += 2;
            }
            "--from" => {
                request.from_ref = Some(
                    tokens
                        .get(index + 1)
                        .ok_or_else(|| anyhow!("missing value for --from"))?
                        .clone(),
                );
                index += 2;
            }
            "--detach" => {
                request.detach = true;
                index += 1;
            }
            "--no-checkout" => {
                request.no_checkout = true;
                index += 1;
            }
            "--lock" => {
                request.lock = true;
                index += 1;
            }
            other => bail!("unknown wt add flag `{other}`"),
        }
    }

    if derive_from_name && request.branch.is_none() {
        request.branch = Some(name_or_path.clone());
        request.path = default_new_worktree_path(name_or_path);
        request.implicit_path_from_branch = true;
    }
    Ok(WorktreeCommand::Add(request))
}

fn default_new_worktree_path(branch_name: &str) -> String {
    format!("../{}", sanitize_branch_name_for_path(branch_name))
}

fn sanitize_branch_name_for_path(branch_name: &str) -> String {
    let mut sanitized = String::with_capacity(branch_name.len());
    for ch in branch_name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            sanitized.push(ch);
        } else {
            sanitized.push('-');
        }
    }
    let sanitized = sanitized.trim_matches('-').trim_matches('.');
    if sanitized.is_empty() {
        "worktree".to_string()
    } else {
        sanitized.to_string()
    }
}

fn resolve_new_worktree_path(repo_root: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    }
}

fn parse_worktree_switch(tokens: &[String]) -> Result<WorktreeCommand> {
    let Some(reference) = tokens.first() else {
        bail!("usage: wt switch <ref> [target] [--create] [--track]");
    };
    let mut request = WorktreeSwitchRequest {
        reference: reference.clone(),
        target: None,
        create: false,
        track: false,
    };
    let mut index = 1usize;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "--create" => {
                request.create = true;
                index += 1;
            }
            "--track" => {
                request.track = true;
                index += 1;
            }
            token => {
                if request.target.is_some() {
                    bail!("usage: wt switch <ref> [target] [--create] [--track]");
                }
                request.target = Some(token.to_string());
                index += 1;
            }
        }
    }
    Ok(WorktreeCommand::Switch(request))
}

fn parse_worktree_move(tokens: &[String]) -> Result<WorktreeCommand> {
    let Some(new_path) = tokens.first() else {
        bail!("usage: wt move <new-path> [target]");
    };
    let request = WorktreeMoveRequest {
        new_path: new_path.clone(),
        target: tokens.get(1).cloned(),
    };
    if tokens.len() > 2 {
        bail!("usage: wt move <new-path> [target]");
    }
    Ok(WorktreeCommand::Move(request))
}

fn parse_worktree_lock(tokens: &[String]) -> Result<WorktreeCommand> {
    let mut request = WorktreeLockRequest {
        target: None,
        reason: None,
    };
    let mut index = 0usize;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "--reason" => {
                let reason = tokens
                    .get(index + 1..)
                    .ok_or_else(|| anyhow!("missing value for --reason"))?;
                if reason.is_empty() {
                    bail!("missing value for --reason");
                }
                request.reason = Some(reason.join(" "));
                break;
            }
            token => {
                if request.target.is_some() {
                    bail!("usage: wt lock [target] [--reason <text>]");
                }
                request.target = Some(token.to_string());
                index += 1;
            }
        }
    }
    Ok(WorktreeCommand::Lock(request))
}

fn parse_worktree_remove(tokens: &[String]) -> Result<WorktreeCommand> {
    let mut request = WorktreeRemoveRequest {
        target: None,
        force: false,
    };
    for token in tokens {
        match token.as_str() {
            "--force" => request.force = true,
            other => {
                if request.target.is_some() {
                    bail!("usage: wt remove [target] [--force]");
                }
                request.target = Some(other.to_string());
            }
        }
    }
    Ok(WorktreeCommand::Remove(request))
}

fn parse_worktree_prune(tokens: &[String]) -> Result<WorktreeCommand> {
    let mut dry_run = false;
    for token in tokens {
        match token.as_str() {
            "--dry-run" | "-n" => dry_run = true,
            other => bail!("unknown wt prune flag `{other}`"),
        }
    }
    Ok(WorktreeCommand::Prune { dry_run })
}

fn parse_worktree_cherry_pick(
    tokens: &[String],
    state: &WorktreeDashboardState,
) -> Result<WorktreeCommand> {
    if tokens.is_empty() {
        bail!("usage: wt cherry-pick <commit-ish...> [target]");
    }
    let mut commits = tokens.to_vec();
    let mut target = None;
    if commits.len() > 1 {
        if let Some(candidate) = commits.last() {
            if worktree_target_matches(state, candidate) {
                target = Some(candidate.clone());
                commits.pop();
            }
        }
    }
    if commits.is_empty() {
        bail!("wt cherry-pick requires at least one commit-ish");
    }
    Ok(WorktreeCommand::CherryPick(WorktreeCherryPickRequest {
        commits,
        target,
    }))
}

fn parse_worktree_ref_command(name: &str, tokens: &[String]) -> Result<WorktreeRefRequest> {
    let Some(reference) = tokens.first() else {
        bail!("usage: wt {name} <ref> [target]");
    };
    if tokens.len() > 2 {
        bail!("usage: wt {name} <ref> [target]");
    }
    Ok(WorktreeRefRequest {
        reference: reference.clone(),
        target: tokens.get(1).cloned(),
    })
}

fn parse_worktree_reset(tokens: &[String]) -> Result<WorktreeCommand> {
    if tokens.len() < 2 || tokens.len() > 3 {
        bail!("usage: wt reset <--soft|--mixed|--hard> <ref> [target]");
    }
    let mode = match tokens[0].as_str() {
        "--soft" => ResetMode::Soft,
        "--mixed" => ResetMode::Mixed,
        "--hard" => ResetMode::Hard,
        other => bail!("unknown reset mode `{other}`"),
    };
    Ok(WorktreeCommand::Reset(WorktreeResetRequest {
        mode,
        reference: tokens[1].clone(),
        target: tokens.get(2).cloned(),
    }))
}

fn parse_worktree_sync(tokens: &[String]) -> Result<WorktreeCommand> {
    let mut request = WorktreeSyncRequest {
        target: None,
        from_ref: None,
        mode: WorktreeSyncMode::Rebase,
        all: false,
        include_dirty: false,
        include_main: false,
    };

    let mut index = 0usize;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "all" | "--all" => {
                if request.target.is_some() {
                    bail!(
                        "usage: wt sync [all|target] [--from ref] [--mode rebase|merge] [--include-dirty] [--include-main]"
                    );
                }
                request.all = true;
                index += 1;
            }
            "--from" => {
                request.from_ref = Some(
                    tokens
                        .get(index + 1)
                        .ok_or_else(|| anyhow!("missing value for --from"))?
                        .clone(),
                );
                index += 2;
            }
            "--mode" => {
                let value = tokens
                    .get(index + 1)
                    .ok_or_else(|| anyhow!("missing value for --mode"))?;
                request.mode = WorktreeSyncMode::parse(value)?;
                index += 2;
            }
            "--merge" => {
                request.mode = WorktreeSyncMode::Merge;
                index += 1;
            }
            "--rebase" => {
                request.mode = WorktreeSyncMode::Rebase;
                index += 1;
            }
            "--include-dirty" => {
                request.include_dirty = true;
                index += 1;
            }
            "--include-main" => {
                request.include_main = true;
                index += 1;
            }
            token => {
                if request.target.is_some() || request.all {
                    bail!(
                        "usage: wt sync [all|target] [--from ref] [--mode rebase|merge] [--include-dirty] [--include-main]"
                    );
                }
                request.target = Some(token.to_string());
                index += 1;
            }
        }
    }

    Ok(WorktreeCommand::Sync(request))
}

fn parse_worktree_task(tokens: &[String]) -> Result<WorktreeCommand> {
    let Some(preset_raw) = tokens.first() else {
        bail!("usage: wt task <test|lint|build> [target]");
    };
    let preset = WorktreeTaskPreset::parse(preset_raw)?;
    parse_worktree_task_alias(preset, &tokens[1..])
}

fn parse_worktree_task_alias(
    preset: WorktreeTaskPreset,
    tokens: &[String],
) -> Result<WorktreeCommand> {
    if tokens.len() > 1 {
        bail!("usage: wt {} [target]", preset.label());
    }
    Ok(WorktreeCommand::Task(WorktreeTaskRequest {
        preset,
        target: tokens.first().cloned(),
    }))
}

fn parse_worktree_sort(rest: &str) -> Result<WorktreeSort> {
    let tokens: Vec<_> = rest.split_whitespace().collect();
    let Some(key_token) = tokens.first() else {
        bail!("usage: sort <path|branch|sync|changes|state|stale> [asc|desc]");
    };
    let key = match key_token.to_ascii_lowercase().as_str() {
        "path" => WorktreeSortKey::Path,
        "branch" => WorktreeSortKey::Branch,
        "sync" => WorktreeSortKey::Sync,
        "changes" => WorktreeSortKey::Changes,
        "state" => WorktreeSortKey::State,
        "stale" => WorktreeSortKey::Stale,
        other => bail!("unknown sort key `{other}`"),
    };
    let direction = match tokens.get(1).map(|value| value.to_ascii_lowercase()) {
        None => SortDirection::Asc,
        Some(value) if value == "asc" => SortDirection::Asc,
        Some(value) if value == "desc" => SortDirection::Desc,
        Some(value) => bail!("unknown sort direction `{}`", value),
    };
    Ok(WorktreeSort { key, direction })
}

fn execute_select_worktree_command(target: &str, state: &mut CockpitState) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, Some(target))?;
    if let Some(index) = state
        .worktrees
        .rows
        .iter()
        .position(|candidate| candidate.key() == row.key())
    {
        state.worktrees.selected = index;
    }
    *state.active_message_mut() =
        DashboardMessage::info(format!("Selected {}.", display_path(&row.worktree.path)));
    Ok(())
}

fn execute_open_worktree_command(target: Option<&str>, state: &mut CockpitState) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, target)?;
    open_path(&row.worktree.path)?;
    *state.active_message_mut() =
        DashboardMessage::success(format!("Opened {}.", display_path(&row.worktree.path)));
    Ok(())
}

fn queue_remove_worktree(state: &mut CockpitState, request: &WorktreeRemoveRequest) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    if row.worktree.is_main {
        bail!("cannot remove the main worktree");
    }
    let safety_risks = worktree_remove_safety_risks(&row.worktree);
    if !safety_risks.is_empty() && !request.force {
        bail!(
            "remove blocked: {}. rerun with `--force` to acknowledge risk",
            safety_risks.join(", ")
        );
    }
    let risk_note = if safety_risks.is_empty() {
        String::new()
    } else {
        format!(" Risks: {}.", safety_risks.join(", "))
    };
    state.pending_confirmation = Some(PendingConfirmation {
        prompt: format!(
            "Remove worktree {}?{}",
            display_path(&row.worktree.path),
            risk_note
        ),
        action: ConfirmableAction::RemoveWorktree {
            target: row.worktree.path.display().to_string(),
            force: request.force,
        },
    });
    *state.active_message_mut() =
        DashboardMessage::info("Pending confirmation. Type `yes` or `no`.");
    Ok(())
}

fn worktree_remove_safety_risks(worktree: &WorktreeRecord) -> Vec<String> {
    let mut risks = Vec::new();
    if worktree.locked {
        risks.push("locked".to_string());
    }
    if worktree.is_dirty() {
        risks.push(format!("dirty:{} files", worktree.change_count()));
    }
    if !worktree.operations.is_empty() {
        let operations = worktree
            .operations
            .iter()
            .map(|op| op.label())
            .collect::<Vec<_>>()
            .join("+");
        risks.push(format!("in-progress:{operations}"));
    }
    if let Some(unpushed) = worktree.unpushed_count() {
        if unpushed > 0 {
            risks.push(format!("unpushed:{unpushed}"));
        }
    }
    if matches!(worktree.merged_into_base, Some(false)) {
        risks.push(format!(
            "unmerged:{}",
            worktree.base_ref.as_deref().unwrap_or("base")
        ));
    }
    risks
}

fn queue_cherry_pick_worktree(
    state: &mut CockpitState,
    request: &WorktreeCherryPickRequest,
) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    state.pending_confirmation = Some(PendingConfirmation {
        prompt: format!(
            "Cherry-pick {} into {}?",
            request.commits.join(" "),
            display_path(&row.worktree.path)
        ),
        action: ConfirmableAction::CherryPickWorktree {
            target: row.worktree.path.display().to_string(),
            commits: request.commits.clone(),
        },
    });
    *state.active_message_mut() =
        DashboardMessage::info("Pending confirmation. Type `yes` or `no`.");
    Ok(())
}

fn queue_merge_worktree(state: &mut CockpitState, request: &WorktreeRefRequest) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    state.pending_confirmation = Some(PendingConfirmation {
        prompt: format!(
            "Merge {} into {}?",
            request.reference,
            display_path(&row.worktree.path)
        ),
        action: ConfirmableAction::MergeWorktree {
            target: row.worktree.path.display().to_string(),
            reference: request.reference.clone(),
        },
    });
    *state.active_message_mut() =
        DashboardMessage::info("Pending confirmation. Type `yes` or `no`.");
    Ok(())
}

fn queue_rebase_worktree(state: &mut CockpitState, request: &WorktreeRefRequest) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    state.pending_confirmation = Some(PendingConfirmation {
        prompt: format!(
            "Rebase {} onto {}?",
            display_path(&row.worktree.path),
            request.reference
        ),
        action: ConfirmableAction::RebaseWorktree {
            target: row.worktree.path.display().to_string(),
            reference: request.reference.clone(),
        },
    });
    *state.active_message_mut() =
        DashboardMessage::info("Pending confirmation. Type `yes` or `no`.");
    Ok(())
}

fn queue_reset_worktree(state: &mut CockpitState, request: &WorktreeResetRequest) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    state.pending_confirmation = Some(PendingConfirmation {
        prompt: format!(
            "Reset {} to {} ({})?",
            display_path(&row.worktree.path),
            request.reference,
            request.mode.label()
        ),
        action: ConfirmableAction::ResetWorktree {
            target: row.worktree.path.display().to_string(),
            mode: request.mode,
            reference: request.reference.clone(),
        },
    });
    *state.active_message_mut() =
        DashboardMessage::info("Pending confirmation. Type `yes` or `no`.");
    Ok(())
}

fn perform_add_worktree(state: &mut CockpitState, request: &WorktreeAddRequest) -> Result<()> {
    let repo = require_repo_snapshot(state)?;
    let target_path = resolve_new_worktree_path(&repo.root, &request.path);
    let mut args = vec!["worktree".to_string(), "add".to_string()];
    if let Some(branch) = request.branch.as_ref() {
        args.push("-b".to_string());
        args.push(branch.clone());
    }
    if request.detach {
        args.push("--detach".to_string());
    }
    if request.no_checkout {
        args.push("--no-checkout".to_string());
    }
    if request.lock {
        args.push("--lock".to_string());
    }
    args.push(target_path.display().to_string());
    if let Some(from_ref) = request.from_ref.as_ref() {
        args.push(from_ref.clone());
    }
    run_git(&repo.root, &args)?;
    let target_display = target_path.display().to_string();
    refresh_worktrees_with_selection(state, Some(&target_display))?;
    *state.active_message_mut() = if request.implicit_path_from_branch {
        DashboardMessage::success(format!(
            "Added worktree {} for branch {}.",
            display_path(&target_path),
            request.branch.as_deref().unwrap_or("unknown")
        ))
    } else {
        DashboardMessage::success(format!("Added worktree {}.", display_path(&target_path)))
    };
    Ok(())
}

fn perform_switch_worktree(
    state: &mut CockpitState,
    request: &WorktreeSwitchRequest,
) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    let mut args = vec!["switch".to_string()];
    if request.create {
        args.push("--create".to_string());
    }
    if request.track {
        args.push("--track".to_string());
    }
    args.push(request.reference.clone());
    run_git(&row.worktree.path, &args)?;
    refresh_worktrees_with_selection(state, Some(&row.worktree.path.display().to_string()))?;
    *state.active_message_mut() = DashboardMessage::success(format!(
        "Switched {} to {}.",
        display_path(&row.worktree.path),
        request.reference
    ));
    Ok(())
}

fn perform_move_worktree(state: &mut CockpitState, request: &WorktreeMoveRequest) -> Result<()> {
    let repo = require_repo_snapshot(state)?;
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    let args = vec![
        "worktree".to_string(),
        "move".to_string(),
        row.worktree.path.display().to_string(),
        request.new_path.clone(),
    ];
    run_git(&repo.root, &args)?;
    refresh_worktrees_with_selection(state, Some(&request.new_path))?;
    *state.active_message_mut() = DashboardMessage::success(format!(
        "Moved {} to {}.",
        display_path(&row.worktree.path),
        request.new_path
    ));
    Ok(())
}

fn perform_lock_worktree(state: &mut CockpitState, request: &WorktreeLockRequest) -> Result<()> {
    let repo = require_repo_snapshot(state)?;
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    let mut args = vec!["worktree".to_string(), "lock".to_string()];
    if let Some(reason) = request.reason.as_ref() {
        args.push("--reason".to_string());
        args.push(reason.clone());
    }
    args.push(row.worktree.path.display().to_string());
    run_git(&repo.root, &args)?;
    refresh_worktrees_with_selection(state, Some(&row.worktree.path.display().to_string()))?;
    *state.active_message_mut() =
        DashboardMessage::success(format!("Locked {}.", display_path(&row.worktree.path)));
    Ok(())
}

fn perform_unlock_worktree(state: &mut CockpitState, target: Option<&str>) -> Result<()> {
    let repo = require_repo_snapshot(state)?;
    let row = resolve_worktree_target(&state.worktrees, target)?;
    let args = vec![
        "worktree".to_string(),
        "unlock".to_string(),
        row.worktree.path.display().to_string(),
    ];
    run_git(&repo.root, &args)?;
    refresh_worktrees_with_selection(state, Some(&row.worktree.path.display().to_string()))?;
    *state.active_message_mut() =
        DashboardMessage::success(format!("Unlocked {}.", display_path(&row.worktree.path)));
    Ok(())
}

fn perform_remove_worktree(
    state: &mut CockpitState,
    request: &WorktreeRemoveRequest,
) -> Result<()> {
    let repo = require_repo_snapshot(state)?;
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    if row.worktree.is_main {
        bail!("cannot remove the main worktree");
    }
    let safety_risks = worktree_remove_safety_risks(&row.worktree);
    if !safety_risks.is_empty() && !request.force {
        bail!(
            "remove blocked: {}. rerun with `--force`",
            safety_risks.join(", ")
        );
    }
    let mut args = vec!["worktree".to_string(), "remove".to_string()];
    if request.force {
        args.push("--force".to_string());
        if row.worktree.locked {
            args.push("--force".to_string());
        }
    }
    args.push(row.worktree.path.display().to_string());
    run_git(&repo.root, &args)?;
    refresh_worktrees_with_selection(state, None)?;
    *state.active_message_mut() =
        DashboardMessage::success(format!("Removed {}.", display_path(&row.worktree.path)));
    Ok(())
}

fn perform_prune_worktrees(state: &mut CockpitState, dry_run: bool) -> Result<()> {
    let repo = require_repo_snapshot(state)?;
    let mut args = vec!["worktree".to_string(), "prune".to_string()];
    if dry_run {
        args.push("--dry-run".to_string());
    }
    run_git(&repo.root, &args)?;
    refresh_worktrees_with_selection(state, None)?;
    *state.active_message_mut() = DashboardMessage::success(if dry_run {
        "Ran git worktree prune --dry-run."
    } else {
        "Pruned stale git worktree metadata."
    });
    Ok(())
}

fn perform_cherry_pick_worktree(
    state: &mut CockpitState,
    request: &WorktreeCherryPickRequest,
) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    let mut args = vec!["cherry-pick".to_string()];
    args.extend(request.commits.iter().cloned());
    run_git(&row.worktree.path, &args)?;
    refresh_worktrees_with_selection(state, Some(&row.worktree.path.display().to_string()))?;
    *state.active_message_mut() = DashboardMessage::success(format!(
        "Cherry-picked into {}.",
        display_path(&row.worktree.path)
    ));
    Ok(())
}

fn perform_merge_worktree(state: &mut CockpitState, request: &WorktreeRefRequest) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    let args = vec!["merge".to_string(), request.reference.clone()];
    run_git(&row.worktree.path, &args)?;
    refresh_worktrees_with_selection(state, Some(&row.worktree.path.display().to_string()))?;
    *state.active_message_mut() = DashboardMessage::success(format!(
        "Merged {} into {}.",
        request.reference,
        display_path(&row.worktree.path)
    ));
    Ok(())
}

fn perform_rebase_worktree(state: &mut CockpitState, request: &WorktreeRefRequest) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    let args = vec!["rebase".to_string(), request.reference.clone()];
    run_git(&row.worktree.path, &args)?;
    refresh_worktrees_with_selection(state, Some(&row.worktree.path.display().to_string()))?;
    *state.active_message_mut() = DashboardMessage::success(format!(
        "Rebased {} onto {}.",
        display_path(&row.worktree.path),
        request.reference
    ));
    Ok(())
}

fn perform_reset_worktree(state: &mut CockpitState, request: &WorktreeResetRequest) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    let args = vec![
        "reset".to_string(),
        request.mode.flag().to_string(),
        request.reference.clone(),
    ];
    run_git(&row.worktree.path, &args)?;
    refresh_worktrees_with_selection(state, Some(&row.worktree.path.display().to_string()))?;
    *state.active_message_mut() = DashboardMessage::success(format!(
        "Reset {} to {} ({}).",
        display_path(&row.worktree.path),
        request.reference,
        request.mode.label()
    ));
    Ok(())
}

fn perform_continue_worktree(state: &mut CockpitState, target: Option<&str>) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, target)?;
    let operation = active_operation(&row.worktree)
        .ok_or_else(|| anyhow!("no single in-progress operation to continue"))?;
    let args = args_for_worktree_resume(operation, true);
    run_git(&row.worktree.path, &args)?;
    refresh_worktrees_with_selection(state, Some(&row.worktree.path.display().to_string()))?;
    *state.active_message_mut() = DashboardMessage::success(format!(
        "Continued {} in {}.",
        operation.label(),
        display_path(&row.worktree.path)
    ));
    Ok(())
}

fn perform_abort_worktree(state: &mut CockpitState, target: Option<&str>) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, target)?;
    let operation = active_operation(&row.worktree)
        .ok_or_else(|| anyhow!("no single in-progress operation to abort"))?;
    let args = args_for_worktree_resume(operation, false);
    run_git(&row.worktree.path, &args)?;
    refresh_worktrees_with_selection(state, Some(&row.worktree.path.display().to_string()))?;
    *state.active_message_mut() = DashboardMessage::success(format!(
        "Aborted {} in {}.",
        operation.label(),
        display_path(&row.worktree.path)
    ));
    Ok(())
}

#[derive(Debug, Default)]
struct WorktreeSyncSummary {
    updated: Vec<String>,
    skipped: Vec<(String, String)>,
    failed: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct CommandRunResult {
    success: bool,
    exit_code: i32,
    duration: Duration,
    output_tail: Vec<String>,
}

fn perform_sync_worktrees(state: &mut CockpitState, request: &WorktreeSyncRequest) -> Result<()> {
    let repo = require_repo_snapshot(state)?;
    let from_ref = request
        .from_ref
        .clone()
        .or_else(|| repo.base_ref.clone())
        .unwrap_or_else(|| "main".to_string());
    let preferred = state.worktrees.selected_key();

    let rows = if request.all {
        state.worktrees.all_rows.clone()
    } else {
        vec![resolve_worktree_target(
            &state.worktrees,
            request.target.as_deref(),
        )?]
    };
    if rows.is_empty() {
        bail!("no worktrees are available to sync");
    }

    let mut summary = WorktreeSyncSummary::default();
    for row in rows {
        let label = display_path(&row.worktree.path);
        if !request.include_main && row.worktree.is_main {
            summary.skipped.push((label, "main".to_string()));
            continue;
        }
        if row.worktree.has_in_progress_operation() {
            let ops = row
                .worktree
                .operations
                .iter()
                .map(|operation| operation.label())
                .collect::<Vec<_>>()
                .join("+");
            summary.skipped.push((label, format!("in-progress:{ops}")));
            continue;
        }
        if !request.include_dirty && row.worktree.is_dirty() {
            summary.skipped.push((label, "dirty".to_string()));
            continue;
        }
        if row.worktree.prunable {
            summary.skipped.push((label, "prunable".to_string()));
            continue;
        }

        let args = match request.mode {
            WorktreeSyncMode::Rebase => vec!["rebase".to_string(), from_ref.clone()],
            WorktreeSyncMode::Merge => vec![
                "merge".to_string(),
                "--ff-only".to_string(),
                from_ref.clone(),
            ],
        };
        let result = run_command_with_capture(&row.worktree.path, "git", &args)
            .with_context(|| format!("failed to execute sync for {}", label))?;
        if result.success {
            summary.updated.push(label);
        } else {
            let reason = result
                .output_tail
                .first()
                .cloned()
                .unwrap_or_else(|| format!("exit {}", result.exit_code));
            summary.failed.push((label, reason));
        }
    }

    refresh_worktrees_with_selection(state, preferred.as_deref())?;

    let mut message = format!(
        "Sync {} from {}: updated {}, skipped {}, failed {}.",
        request.mode.label(),
        from_ref,
        summary.updated.len(),
        summary.skipped.len(),
        summary.failed.len()
    );
    if let Some((path, reason)) = summary.failed.first() {
        message.push_str(&format!(
            " first failure: {path} ({})",
            trim_middle(reason, 50)
        ));
    }
    state.worktrees.message = if summary.failed.is_empty() {
        DashboardMessage::success(message)
    } else {
        DashboardMessage::error(message)
    };
    Ok(())
}

fn perform_worktree_task(state: &mut CockpitState, request: &WorktreeTaskRequest) -> Result<()> {
    let row = resolve_worktree_target(&state.worktrees, request.target.as_deref())?;
    let command =
        resolve_worktree_task_command(&row.worktree.path, request.preset).with_context(|| {
            format!(
                "could not resolve `{}` preset for {}",
                request.preset.label(),
                display_path(&row.worktree.path)
            )
        })?;
    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("task command is empty"))?;
    let result = run_command_with_capture(&row.worktree.path, program, args)?;

    refresh_worktrees_with_selection(state, Some(&row.worktree.path.display().to_string()))?;
    let command_display = command.join(" ");
    state.worktrees.last_task = Some(WorktreeTaskResult {
        target_path: row.worktree.path.clone(),
        preset: request.preset,
        command: command_display.clone(),
        success: result.success,
        exit_code: result.exit_code,
        duration: result.duration,
        output_tail: result.output_tail.clone(),
        finished_at_epoch: now_epoch(),
    });
    state.worktrees.message = if result.success {
        DashboardMessage::success(format!(
            "{} passed in {} for {} ({})",
            request.preset.label(),
            format_duration_short(result.duration),
            display_path(&row.worktree.path),
            command_display
        ))
    } else {
        let output = result
            .output_tail
            .first()
            .cloned()
            .unwrap_or_else(|| format!("exit {}", result.exit_code));
        DashboardMessage::error(format!(
            "{} failed (exit {}) for {}: {}",
            request.preset.label(),
            result.exit_code,
            display_path(&row.worktree.path),
            trim_middle(&output, 80)
        ))
    };
    Ok(())
}

fn resolve_worktree_task_command(path: &Path, preset: WorktreeTaskPreset) -> Result<Vec<String>> {
    if let Some(node_command) = resolve_node_task_command(path, preset)? {
        return Ok(node_command);
    }

    if path.join("Cargo.toml").exists() {
        let command = match preset {
            WorktreeTaskPreset::Test => vec!["cargo".to_string(), "test".to_string()],
            WorktreeTaskPreset::Lint => vec![
                "cargo".to_string(),
                "clippy".to_string(),
                "--all-targets".to_string(),
                "--all-features".to_string(),
            ],
            WorktreeTaskPreset::Build => vec!["cargo".to_string(), "build".to_string()],
        };
        return Ok(command);
    }

    if path.join("go.mod").exists() {
        let command = match preset {
            WorktreeTaskPreset::Test => {
                vec!["go".to_string(), "test".to_string(), "./...".to_string()]
            }
            WorktreeTaskPreset::Lint => {
                vec!["go".to_string(), "vet".to_string(), "./...".to_string()]
            }
            WorktreeTaskPreset::Build => {
                vec!["go".to_string(), "build".to_string(), "./...".to_string()]
            }
        };
        return Ok(command);
    }

    if path.join("pyproject.toml").exists() {
        let command = match preset {
            WorktreeTaskPreset::Test => {
                vec![
                    "python3".to_string(),
                    "-m".to_string(),
                    "pytest".to_string(),
                ]
            }
            WorktreeTaskPreset::Lint => vec![
                "python3".to_string(),
                "-m".to_string(),
                "ruff".to_string(),
                "check".to_string(),
                ".".to_string(),
            ],
            WorktreeTaskPreset::Build => {
                vec!["python3".to_string(), "-m".to_string(), "build".to_string()]
            }
        };
        return Ok(command);
    }

    bail!(
        "no preset runner found for {} in {}",
        preset.label(),
        display_path(path)
    )
}

fn resolve_node_task_command(
    path: &Path,
    preset: WorktreeTaskPreset,
) -> Result<Option<Vec<String>>> {
    let package_json = path.join("package.json");
    if !package_json.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&package_json)
        .with_context(|| format!("failed to read {}", package_json.display()))?;
    let parsed: Value =
        serde_json::from_str(&raw).context("failed to parse package.json while resolving task")?;
    let has_script = parsed
        .get("scripts")
        .and_then(Value::as_object)
        .map(|scripts| scripts.contains_key(preset.label()))
        .unwrap_or(false);
    if !has_script {
        return Ok(None);
    }

    let command = if path.join("pnpm-lock.yaml").exists() || path.join("pnpm-lock.yml").exists() {
        vec![
            "pnpm".to_string(),
            "run".to_string(),
            preset.label().to_string(),
        ]
    } else if path.join("yarn.lock").exists() {
        vec!["yarn".to_string(), preset.label().to_string()]
    } else if path.join("bun.lockb").exists() || path.join("bun.lock").exists() {
        vec![
            "bun".to_string(),
            "run".to_string(),
            preset.label().to_string(),
        ]
    } else {
        vec![
            "npm".to_string(),
            "run".to_string(),
            preset.label().to_string(),
        ]
    };

    Ok(Some(command))
}

fn run_command_with_capture(
    cwd: &Path,
    program: &str,
    args: &[String],
) -> Result<CommandRunResult> {
    let started = Instant::now();
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to execute `{}` in {}", program, cwd.display()))?;
    let duration = started.elapsed();
    let mut merged = String::new();
    if !output.stdout.is_empty() {
        merged.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str(&String::from_utf8_lossy(&output.stderr));
    }

    Ok(CommandRunResult {
        success: output.status.success(),
        exit_code: output.status.code().unwrap_or(1),
        duration,
        output_tail: render_command_output_tail(&merged, 6),
    })
}

fn render_command_output_tail(output: &str, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| trim_middle(line, 100))
        .collect();
    if lines.len() > max_lines {
        lines = lines.split_off(lines.len() - max_lines);
    }
    lines
}

fn args_for_worktree_resume(operation: WorktreeOperation, continue_op: bool) -> Vec<String> {
    let flag = if continue_op { "--continue" } else { "--abort" };
    match operation {
        WorktreeOperation::Rebase => vec!["rebase".to_string(), flag.to_string()],
        WorktreeOperation::Merge => vec!["merge".to_string(), flag.to_string()],
        WorktreeOperation::CherryPick => vec!["cherry-pick".to_string(), flag.to_string()],
    }
}

fn resolve_worktree_target(
    state: &WorktreeDashboardState,
    target: Option<&str>,
) -> Result<WorktreeRow> {
    let Some(repo) = state.repo.as_ref() else {
        bail!("no git repository is available from the current directory");
    };

    let Some(target) = target else {
        return state
            .selected_row()
            .cloned()
            .or_else(|| state.all_rows.first().cloned())
            .ok_or_else(|| anyhow!("no worktree is selected"));
    };

    let input_path = absolutize_target_path(target)?;
    if let Some(row) = state
        .all_rows
        .iter()
        .find(|row| row.worktree.path == input_path || row.worktree.path == Path::new(target))
    {
        return Ok(row.clone());
    }

    if let Some(row) = state
        .all_rows
        .iter()
        .find(|row| row.worktree.branch.as_deref() == Some(target))
    {
        return Ok(row.clone());
    }

    if repo.worktrees.is_empty() {
        bail!("no git worktrees are available")
    }

    bail!("no worktree matches `{}`", target)
}

fn worktree_target_matches(state: &WorktreeDashboardState, target: &str) -> bool {
    resolve_worktree_target(state, Some(target)).is_ok()
}

fn require_repo_snapshot(state: &mut CockpitState) -> Result<RepoSnapshot> {
    if state.worktrees.repo.is_none() {
        state.worktrees.apply_repo(discover_current_repo()?);
    }
    state
        .worktrees
        .repo
        .clone()
        .ok_or_else(|| anyhow!("no git repository is available from the current directory"))
}

fn refresh_worktrees_with_selection(
    state: &mut CockpitState,
    preferred: Option<&str>,
) -> Result<()> {
    state.worktrees.apply_repo(discover_current_repo()?);
    state.worktrees.rebuild_rows(preferred);
    Ok(())
}

fn absolutize_target_path(target: &str) -> Result<PathBuf> {
    let path = PathBuf::from(target);
    let resolved = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .context("failed to determine current directory")?
            .join(path)
    };
    Ok(fs::canonicalize(&resolved).unwrap_or(resolved))
}

impl ResetMode {
    fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ResetMode::Soft => "soft",
            ResetMode::Mixed => "mixed",
            ResetMode::Hard => "hard",
        }
    }
}

impl WorktreeSyncMode {
    fn parse(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "rebase" => Ok(Self::Rebase),
            "merge" => Ok(Self::Merge),
            other => bail!("unknown sync mode `{other}`"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            WorktreeSyncMode::Rebase => "rebase",
            WorktreeSyncMode::Merge => "merge",
        }
    }
}

impl WorktreeTaskPreset {
    fn parse(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "test" => Ok(Self::Test),
            "lint" => Ok(Self::Lint),
            "build" => Ok(Self::Build),
            other => bail!("unknown task preset `{other}`"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            WorktreeTaskPreset::Test => "test",
            WorktreeTaskPreset::Lint => "lint",
            WorktreeTaskPreset::Build => "build",
        }
    }
}

fn compare_rows_by_sort(
    key: DashboardSortKey,
    direction: SortDirection,
    left: &DashboardRow,
    right: &DashboardRow,
    health_cache: &HashMap<(u16, i32), HealthStatus>,
) -> Ordering {
    let ordering = match key {
        DashboardSortKey::Port => left.port.cmp(&right.port),
        DashboardSortKey::Project => left
            .owner
            .project_name
            .to_ascii_lowercase()
            .cmp(&right.owner.project_name.to_ascii_lowercase()),
        DashboardSortKey::Age => left
            .owner
            .age
            .unwrap_or_default()
            .cmp(&right.owner.age.unwrap_or_default()),
        DashboardSortKey::State => left.owner.stale.cmp(&right.owner.stale),
        DashboardSortKey::Health => {
            let left_rank = health_cache
                .get(&left.key())
                .cloned()
                .unwrap_or(HealthStatus::Unknown)
                .rank();
            let right_rank = health_cache
                .get(&right.key())
                .cloned()
                .unwrap_or(HealthStatus::Unknown)
                .rank();
            left_rank.cmp(&right_rank)
        }
    };

    match direction {
        SortDirection::Asc => ordering,
        SortDirection::Desc => ordering.reverse(),
    }
}

fn parse_session_target(value: Option<&str>, state: &DashboardState) -> Result<SessionTarget> {
    let Some(value) = value else {
        return Ok(SessionTarget::Selected);
    };

    if let Ok(port) = value.parse::<u16>() {
        if state.snapshot.active.iter().any(|entry| entry.port == port) {
            return Ok(SessionTarget::Port(port));
        }
    }

    Ok(SessionTarget::Pid(parse_pid_token(value)?))
}

fn resolve_session_target(
    state: &DashboardState,
    target: SessionTarget,
    require_unique_port: bool,
) -> Result<ResolvedSessionTarget> {
    match target {
        SessionTarget::Selected => {
            let row = state
                .selected_row()
                .cloned()
                .ok_or_else(|| anyhow!("no session is selected"))?;
            Ok(ResolvedSessionTarget {
                pids: vec![row.owner.pid],
                row,
            })
        }
        SessionTarget::Pid(pid) => {
            let row = state
                .all_rows
                .iter()
                .find(|row| row.owner.pid == pid)
                .cloned()
                .ok_or_else(|| anyhow!("pid {} is not currently listening", pid))?;
            Ok(ResolvedSessionTarget {
                pids: vec![pid],
                row,
            })
        }
        SessionTarget::Port(port) => {
            let active = state
                .snapshot
                .active
                .iter()
                .find(|entry| entry.port == port)
                .ok_or_else(|| anyhow!("port {} is not active", port))?;
            if require_unique_port && active.owners.len() != 1 {
                bail!(
                    "port {} has {} owners; target a specific pid",
                    port,
                    active.owners.len()
                );
            }
            let owner = active
                .owners
                .first()
                .ok_or_else(|| anyhow!("port {} has no owners", port))?;
            let row = state
                .all_rows
                .iter()
                .find(|row| row.port == port && row.owner.pid == owner.pid)
                .cloned()
                .ok_or_else(|| anyhow!("failed to resolve row for port {}", port))?;
            Ok(ResolvedSessionTarget {
                pids: unique_pids(active),
                row,
            })
        }
    }
}

enum KillTarget {
    Port(u16),
    Pid(i32),
}

enum SessionTarget {
    Selected,
    Port(u16),
    Pid(i32),
}

#[derive(Clone)]
struct ResolvedSessionTarget {
    row: DashboardRow,
    pids: Vec<i32>,
}

fn execute_kill_command(
    target: &[String],
    state: &mut DashboardState,
    stale_after: Duration,
) -> Result<()> {
    match resolve_kill_target(target, state)? {
        KillTarget::Port(port) => {
            let snapshot = refresh_snapshot(stale_after)?;
            let active = snapshot
                .active
                .iter()
                .find(|entry| entry.port == port)
                .cloned()
                .ok_or_else(|| anyhow!("port {} is already free", port))?;
            let pids = unique_pids(&active);
            terminate_processes(&pids)?;
            if wait_for_port_to_clear(port, Duration::from_secs(2), stale_after)?.is_some() {
                bail!("port {} is still occupied after SIGTERM", port);
            }
            state.apply_snapshot(refresh_snapshot(stale_after)?);
            state.message = DashboardMessage::success(format!(
                "Freed port {} by terminating {}.",
                port,
                describe_pids(&pids)
            ));
        }
        KillTarget::Pid(pid) => {
            terminate_processes(&[pid])?;
            state.apply_snapshot(refresh_snapshot(stale_after)?);
            state.message = DashboardMessage::success(format!("Sent SIGTERM to pid {}.", pid));
        }
    }

    Ok(())
}

fn execute_restart_command(
    request: &RestartRequest,
    state: &mut DashboardState,
    stale_after: Duration,
) -> Result<()> {
    let snapshot = refresh_snapshot(stale_after)?;
    state.apply_snapshot(snapshot);

    let target = parse_session_target(request.target.as_deref(), state)?;
    let resolved = resolve_session_target(state, target, true)?;
    let from_port = resolved.row.port;
    let to_port = request.port_override.unwrap_or(from_port);

    if to_port != from_port
        && state
            .snapshot
            .active
            .iter()
            .any(|entry| entry.port == to_port)
    {
        bail!("port {} is already in use", to_port);
    }

    if to_port == from_port && resolved.row.owner_count > 1 {
        bail!(
            "port {} has multiple owners; restart a pid with `restart <pid> --port <new-port>`",
            from_port
        );
    }

    let cwd = resolved
        .row
        .owner
        .cwd
        .clone()
        .or_else(|| resolved.row.owner.project_root.clone())
        .ok_or_else(|| anyhow!("restart requires a known working directory"))?;

    if to_port == from_port {
        terminate_processes(&resolved.pids)?;
        if wait_for_port_to_clear(from_port, Duration::from_secs(3), stale_after)?.is_some() {
            bail!("port {} is still occupied after SIGTERM", from_port);
        }

        let child_pid = spawn_moved_process(&resolved.row.owner, &cwd, from_port, to_port)?;
        if wait_for_port_to_open(to_port, DASHBOARD_MOVE_WAIT, stale_after)?.is_none() {
            bail!(
                "restarted pid but port {} did not bind within {}",
                to_port,
                format_duration_short(DASHBOARD_MOVE_WAIT)
            );
        }
        state.apply_snapshot(refresh_snapshot(stale_after)?);
        state.message = DashboardMessage::success(format!(
            "Restarted pid {} on port {} as pid {}.",
            resolved.row.owner.pid, to_port, child_pid
        ));
        return Ok(());
    }

    let child_pid = spawn_moved_process(&resolved.row.owner, &cwd, from_port, to_port)?;
    if wait_for_port_to_open(to_port, DASHBOARD_MOVE_WAIT, stale_after)?.is_none() {
        bail!(
            "started replacement on port {} as pid {}, but it never became healthy",
            to_port,
            child_pid
        );
    }

    terminate_processes(&resolved.pids)?;
    if resolved.row.owner_count == 1 {
        let _ = wait_for_port_to_clear(from_port, Duration::from_secs(3), stale_after)?;
    }

    state.apply_snapshot(refresh_snapshot(stale_after)?);
    state.message = DashboardMessage::success(format!(
        "Restarted pid {} from {} to {} as pid {}.",
        resolved.row.owner.pid, from_port, to_port, child_pid
    ));
    Ok(())
}

fn execute_quick_action(
    action: QuickAction,
    state: &mut DashboardState,
    stale_after: Duration,
) -> Result<()> {
    let snapshot = refresh_snapshot(stale_after)?;
    state.apply_snapshot(snapshot);

    match action {
        QuickAction::KillStale => {
            let pids: BTreeSet<i32> = state
                .all_rows
                .iter()
                .filter(|row| row.owner.stale)
                .map(|row| row.owner.pid)
                .collect();
            if pids.is_empty() {
                state.message = DashboardMessage::info("No stale sessions to kill.");
                return Ok(());
            }
            let pid_list: Vec<_> = pids.into_iter().collect();
            terminate_processes(&pid_list)?;
            thread::sleep(Duration::from_millis(250));
            state.apply_snapshot(refresh_snapshot(stale_after)?);
            state.message = DashboardMessage::success(format!(
                "Quick action killed stale {}.",
                describe_pids(&pid_list)
            ));
        }
        QuickAction::KillOld => {
            let pids: BTreeSet<i32> = state
                .all_rows
                .iter()
                .filter(|row| row.owner.age.unwrap_or_default() >= QUICK_ACTION_OLD_AFTER)
                .map(|row| row.owner.pid)
                .collect();
            if pids.is_empty() {
                state.message = DashboardMessage::info("No old sessions to kill.");
                return Ok(());
            }
            let pid_list: Vec<_> = pids.into_iter().collect();
            terminate_processes(&pid_list)?;
            thread::sleep(Duration::from_millis(250));
            state.apply_snapshot(refresh_snapshot(stale_after)?);
            state.message = DashboardMessage::success(format!(
                "Quick action killed old {} (age >= {}).",
                describe_pids(&pid_list),
                format_duration_short(QUICK_ACTION_OLD_AFTER)
            ));
        }
        QuickAction::RestartOld => {
            let pids: BTreeSet<i32> = state
                .all_rows
                .iter()
                .filter(|row| row.owner.age.unwrap_or_default() >= QUICK_ACTION_OLD_AFTER)
                .map(|row| row.owner.pid)
                .collect();
            if pids.is_empty() {
                state.message = DashboardMessage::info("No old sessions to restart.");
                return Ok(());
            }

            let mut restarted = 0usize;
            let mut failures = Vec::new();
            for pid in pids {
                let request = RestartRequest {
                    target: Some(pid.to_string()),
                    port_override: None,
                };
                match execute_restart_command(&request, state, stale_after) {
                    Ok(_) => restarted += 1,
                    Err(error) => failures.push(format!("pid {}: {}", pid, error)),
                }
            }

            if failures.is_empty() {
                state.message = DashboardMessage::success(format!(
                    "Quick action restarted {} old session(s).",
                    restarted
                ));
            } else {
                state.message = DashboardMessage::error(format!(
                    "Quick restart-old: {} restarted, {} failed ({})",
                    restarted,
                    failures.len(),
                    trim_middle(&failures.join("; "), 120)
                ));
            }
        }
    }

    Ok(())
}

fn execute_open_command(target: Option<&str>, state: &mut DashboardState) -> Result<()> {
    let target = parse_session_target(target, state)?;
    let resolved = resolve_session_target(state, target, false)?;
    let url = resolved.row.url();
    let health = probe_port_health(resolved.row.port, Duration::from_millis(300));
    state.apply_health_sample(resolved.row.port, resolved.row.owner.pid, health.clone());
    state.rebuild_rows(Some(resolved.row.key()));
    open_url(&url)?;
    state.message = DashboardMessage::success(format!("Opened {} ({})", url, health.badge()));
    Ok(())
}

fn resolve_kill_target(target: &[String], state: &DashboardState) -> Result<KillTarget> {
    match target {
        [] => state
            .selected_row()
            .map(|row| KillTarget::Port(row.port))
            .ok_or_else(|| anyhow!("no session is selected")),
        [kind, value] if kind.eq_ignore_ascii_case("port") => {
            Ok(KillTarget::Port(parse_port_token(value)?))
        }
        [kind, value] if kind.eq_ignore_ascii_case("pid") => {
            Ok(KillTarget::Pid(parse_pid_token(value)?))
        }
        [value] => {
            if let Ok(port) = value.parse::<u16>() {
                if state.snapshot.active.iter().any(|entry| entry.port == port) {
                    return Ok(KillTarget::Port(port));
                }
            }
            Ok(KillTarget::Pid(parse_pid_token(value)?))
        }
        _ => bail!("usage: kill [3000|port 3000|pid 1234]"),
    }
}

fn execute_move_command(
    from: Option<u16>,
    to: u16,
    state: &mut DashboardState,
    stale_after: Duration,
) -> Result<()> {
    let from_port = from
        .or_else(|| state.selected_row().map(|row| row.port))
        .ok_or_else(|| anyhow!("usage: move <new-port> or move <from-port> <new-port>"))?;

    if from_port == to {
        bail!("source and destination ports must differ");
    }

    let snapshot = refresh_snapshot(stale_after)?;
    if snapshot.active.iter().any(|entry| entry.port == to) {
        bail!("port {} is already in use", to);
    }

    let active = snapshot
        .active
        .iter()
        .find(|entry| entry.port == from_port)
        .cloned()
        .ok_or_else(|| anyhow!("port {} is not active", from_port))?;

    let owner = state
        .selected_row()
        .filter(|row| row.port == from_port)
        .map(|row| row.owner.clone())
        .or_else(|| active.owners.first().cloned())
        .ok_or_else(|| anyhow!("port {} has no moveable owner", from_port))?;

    let cwd = owner
        .cwd
        .clone()
        .or_else(|| owner.project_root.clone())
        .ok_or_else(|| anyhow!("port {} is missing a working directory", from_port))?;

    let child_pid = spawn_moved_process(&owner, &cwd, from_port, to)?;

    if wait_for_port_to_open(to, DASHBOARD_MOVE_WAIT, stale_after)?.is_none() {
        state.apply_snapshot(refresh_snapshot(stale_after)?);
        bail!(
            "started a replacement listener on port {} as pid {}, but it did not bind within {}. left port {} untouched",
            to,
            child_pid,
            format_duration_short(DASHBOARD_MOVE_WAIT),
            from_port
        );
    }

    let pids = unique_pids(&active);
    terminate_processes(&pids)?;
    if wait_for_port_to_clear(from_port, Duration::from_secs(3), stale_after)?.is_some() {
        state.apply_snapshot(refresh_snapshot(stale_after)?);
        bail!(
            "port {} started on {}, but the old listener is still occupying {}",
            from_port,
            to,
            from_port
        );
    }

    state.apply_snapshot(refresh_snapshot(stale_after)?);
    state.message = DashboardMessage::success(format!(
        "Moved port {} to {} with pid {}.",
        from_port, to, child_pid
    ));
    Ok(())
}

fn spawn_moved_process(
    owner: &ProcessRecord,
    cwd: &Path,
    from_port: u16,
    to_port: u16,
) -> Result<u32> {
    let parts = shell_words::split(&owner.command_line).ok();
    let mut command = if let Some(parts) = parts {
        if let Some((program, args)) = parts.split_first() {
            let mut command = Command::new(program);
            command.args(args);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-lc", &owner.command_line]);
            command
        }
    } else {
        let mut command = Command::new("sh");
        command.args(["-lc", &owner.command_line]);
        command
    };

    let child = command
        .current_dir(cwd)
        .env("PORT", to_port.to_string())
        .env("PORTLEDGER_OLD_PORT", from_port.to_string())
        .env("PORTLEDGER_NEW_PORT", to_port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start replacement for port {}", from_port))?;

    Ok(child.id())
}

fn wait_for_port_to_open(
    port: u16,
    timeout: Duration,
    stale_after: Duration,
) -> Result<Option<PortSnapshot>> {
    let deadline = SystemTime::now() + timeout;
    loop {
        let active = discover_active_ports(stale_after)?;
        if let Some(snapshot) = active.into_iter().find(|entry| entry.port == port) {
            return Ok(Some(snapshot));
        }
        if SystemTime::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn probe_port_health(port: u16, timeout: Duration) -> HealthStatus {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let mut stream = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(stream) => stream,
        Err(error) => return HealthStatus::Down(error.to_string()),
    };

    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request = b"GET / HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";

    if stream.write_all(request).is_err() {
        return HealthStatus::Tcp;
    }

    let mut buffer = [0u8; 256];
    let bytes = match stream.read(&mut buffer) {
        Ok(bytes) => bytes,
        Err(_) => return HealthStatus::Tcp,
    };
    if bytes == 0 {
        return HealthStatus::Tcp;
    }

    let header = String::from_utf8_lossy(&buffer[..bytes]);
    let status_line = header.lines().next().unwrap_or_default();
    if let Some(code) = parse_http_status_code(status_line) {
        HealthStatus::Up(code)
    } else {
        HealthStatus::Tcp
    }
}

fn parse_http_status_code(status_line: &str) -> Option<u16> {
    let mut parts = status_line.split_whitespace();
    let protocol = parts.next()?;
    if !protocol.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse::<u16>().ok()
}

fn open_url(url: &str) -> Result<()> {
    open_target(url)
}

fn open_path(path: &Path) -> Result<()> {
    open_target(path)
}

fn open_target(target: impl AsRef<std::ffi::OsStr>) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else {
        bail!("open is not supported on this platform");
    };

    let status = Command::new(program)
        .arg(target)
        .status()
        .with_context(|| format!("failed to execute {}", program))?;
    if !status.success() {
        bail!("{} returned non-zero status", program);
    }
    Ok(())
}

fn execute_select_command(target: &str, state: &mut DashboardState) -> Result<()> {
    if let Ok(port) = target.parse::<u16>() {
        if let Some(index) = state.rows.iter().position(|row| row.port == port) {
            state.selected = index;
            state.message = DashboardMessage::info(format!("Selected port {}.", port));
            return Ok(());
        }
    }

    let pid = parse_pid_token(target)?;
    if let Some(index) = state.rows.iter().position(|row| row.owner.pid == pid) {
        state.selected = index;
        state.message = DashboardMessage::info(format!("Selected pid {}.", pid));
        return Ok(());
    }

    bail!("no session matches `{}`", target)
}

fn build_dashboard_rows(snapshot: &Snapshot) -> Vec<DashboardRow> {
    let mut rows = Vec::new();

    for entry in &snapshot.active {
        let history = snapshot.state.ports.get(&entry.port).cloned();
        for owner in &entry.owners {
            rows.push(DashboardRow {
                port: entry.port,
                owner: owner.clone(),
                owner_count: entry.owners.len(),
                history: history.clone(),
            });
        }
    }

    rows
}

fn render_dashboard(stdout: &mut io::Stdout, state: &CockpitState, args: &MapArgs) -> Result<()> {
    let (width, height) = terminal::size().context("failed to read terminal size")?;
    let width = width as usize;
    let height = height as usize;
    let lines = build_dashboard_lines(state, args, width, height);
    let prompt_prefix = "Command > ";
    let input_width = width.saturating_sub(prompt_prefix.chars().count());
    let prompt_value = visible_input_tail(&state.input, input_width);
    let prompt_row = height.saturating_sub(1) as u16;
    let prompt_col =
        (prompt_prefix.chars().count() + prompt_value.chars().count()).min(width.saturating_sub(1));

    queue!(stdout, MoveTo(0, 0), Clear(ClearType::All)).context("failed to clear dashboard")?;
    for (index, line) in lines.iter().enumerate().take(height) {
        queue!(stdout, MoveTo(0, index as u16)).context("failed to position cursor")?;
        write!(stdout, "{}", truncate_end(line, width)).context("failed to draw dashboard line")?;
    }
    queue!(stdout, MoveTo(prompt_col as u16, prompt_row), Show)
        .context("failed to position prompt")?;
    stdout.flush().context("failed to flush dashboard")?;
    Ok(())
}

fn build_dashboard_lines(
    state: &CockpitState,
    args: &MapArgs,
    width: usize,
    height: usize,
) -> Vec<String> {
    let has_rows = match state.view {
        CockpitView::Ports => !state.ports.rows.is_empty(),
        CockpitView::Worktrees => !state.worktrees.rows.is_empty(),
    };
    let (list_height, detail_height) = dashboard_layout(height, has_rows);
    let list_inner_width = width.saturating_sub(2);

    let mut lines = Vec::with_capacity(height);
    lines.push(box_top(width, &format!("Cockpit ({})", state.view.label())));
    let overview = build_overview_lines(state, args);
    for line in overview {
        lines.push(box_line(width, &line));
    }
    lines.push(box_bottom(width));

    lines.push(box_top(width, &active_list_title(state)));
    lines.push(box_line(
        width,
        &active_list_header(state, list_inner_width),
    ));
    lines.extend(build_list_lines(
        state,
        width,
        list_inner_width,
        list_height,
    ));
    lines.push(box_bottom(width));

    lines.push(box_top(
        width,
        if state.show_command_menu {
            "Command Menu"
        } else {
            "Details"
        },
    ));
    let detail_lines = if state.show_command_menu {
        build_command_menu_lines(state.view)
    } else {
        build_detail_lines(state)
    };
    for index in 0..detail_height {
        lines.push(box_line(
            width,
            detail_lines
                .get(index)
                .map(String::as_str)
                .unwrap_or_default(),
        ));
    }
    lines.push(box_bottom(width));

    lines.push("Hotkeys: Tab switch view, ?/F1 menu, Esc clear/cancel, Ctrl+C quit".to_string());
    let status = state
        .pending_confirmation
        .as_ref()
        .map(|pending| format!("[confirm] {} (yes/no)", pending.prompt))
        .unwrap_or_else(|| state.active_message().render());
    lines.push(format!("Status: {}", status));
    lines.push(format!(
        "Command > {}",
        visible_input_tail(&state.input, width.saturating_sub(10))
    ));

    lines.truncate(height);
    while lines.len() < height {
        lines.push(String::new());
    }
    lines
}

fn build_dashboard_summary(snapshot: &Snapshot) -> DashboardExportSummary {
    let processes = snapshot
        .active
        .iter()
        .map(|entry| entry.owners.len())
        .sum::<usize>();
    let stale = snapshot
        .active
        .iter()
        .flat_map(|entry| &entry.owners)
        .filter(|owner| owner.stale)
        .count();
    let projects = snapshot
        .active
        .iter()
        .flat_map(|entry| &entry.owners)
        .map(|owner| {
            owner
                .project_root
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| owner.project_name.clone())
        })
        .collect::<BTreeSet<_>>()
        .len();

    DashboardExportSummary {
        ports: snapshot.active.len(),
        processes,
        stale,
        projects,
    }
}

fn build_worktree_summary(repo: Option<&RepoSnapshot>) -> Option<DashboardExportRepo> {
    let repo = repo?;
    Some(DashboardExportRepo {
        root: repo.root.clone(),
        common_dir: repo.common_dir.clone(),
        worktree_count: repo.worktrees.len(),
        dirty: repo.worktrees.iter().filter(|item| item.is_dirty()).count(),
        locked: repo.worktrees.iter().filter(|item| item.locked).count(),
        prunable: repo.worktrees.iter().filter(|item| item.prunable).count(),
        detached: repo.worktrees.iter().filter(|item| item.detached).count(),
    })
}

fn build_overview_lines(state: &CockpitState, args: &MapArgs) -> Vec<String> {
    match state.view {
        CockpitView::Ports => {
            let summary = build_dashboard_summary(&state.ports.snapshot);
            let free_port_list = free_ports(
                DEFAULT_AVAILABLE_FROM,
                DEFAULT_AVAILABLE_TO,
                &state.ports.snapshot.active,
                8,
            );
            let free_ports = if free_port_list.is_empty() {
                "none".to_string()
            } else {
                free_port_list
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let released_count = state
                .ports
                .snapshot
                .state
                .ports
                .values()
                .filter(|entry| entry.last_status == PersistedStatus::Released)
                .count();
            vec![
                format!(
                    "ports:{}  processes:{}  stale:{}  projects:{}",
                    summary.ports, summary.processes, summary.stale, summary.projects
                ),
                format!(
                    "free:{}  released:{}  refresh:{}  stale-after:{}",
                    free_ports,
                    released_count,
                    format_duration_short(args.refresh_every),
                    format_duration_short(args.stale_after)
                ),
                format!(
                    "filter:{}  sort:{}  visible:{}/{}  Tab -> Worktrees",
                    state.ports.filter_label(),
                    state.ports.sort_label(),
                    state.ports.rows.len(),
                    state.ports.all_rows.len()
                ),
            ]
        }
        CockpitView::Worktrees => {
            let summary = build_worktree_summary(state.worktrees.repo.as_ref());
            match summary {
                Some(summary) => vec![
                    format!(
                        "repo:{}  worktrees:{}  dirty:{}  locked:{}",
                        display_path(&summary.root),
                        summary.worktree_count,
                        summary.dirty,
                        summary.locked
                    ),
                    format!(
                        "detached:{}  prunable:{}  refresh:{}  common:{}",
                        summary.detached,
                        summary.prunable,
                        format_duration_short(args.refresh_every),
                        display_path(&summary.common_dir)
                    ),
                    format!(
                        "filter:{}  sort:{}  visible:{}/{}  Tab -> Ports",
                        state.worktrees.filter_label(),
                        state.worktrees.sort_label(),
                        state.worktrees.rows.len(),
                        state.worktrees.all_rows.len()
                    ),
                ],
                None => vec![
                    "repo:none  worktrees:0  dirty:0  locked:0".to_string(),
                    format!(
                        "detached:0  prunable:0  refresh:{}  current:{}",
                        format_duration_short(args.refresh_every),
                        display_path(
                            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                        )
                    ),
                    "filter:none  sort:state desc  visible:0/0  Tab -> Ports".to_string(),
                ],
            }
        }
    }
}

fn box_top(width: usize, title: &str) -> String {
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "┌".to_string();
    }

    let inner = width.saturating_sub(2);
    let label = truncate_end(&format!(" {} ", title), inner);
    let trailing = inner.saturating_sub(label.chars().count());
    format!("┌{}{}┐", label, "─".repeat(trailing))
}

fn box_bottom(width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "└".to_string();
    }

    format!("└{}┘", "─".repeat(width.saturating_sub(2)))
}

fn box_line(width: usize, content: &str) -> String {
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "│".to_string();
    }

    let inner = width.saturating_sub(2);
    let body = truncate_end(content, inner);
    let padding = inner.saturating_sub(body.chars().count());
    format!("│{}{}│", body, " ".repeat(padding))
}

fn build_detail_lines(state: &CockpitState) -> Vec<String> {
    match state.view {
        CockpitView::Ports => build_port_detail_lines(&state.ports),
        CockpitView::Worktrees => build_worktree_detail_lines(&state.worktrees),
    }
}

fn build_port_detail_lines(state: &DashboardState) -> Vec<String> {
    let Some(row) = state.selected_row() else {
        let mut lines = vec![
            "No selection.".to_string(),
            format!(
                "Download target: {}/{}",
                display_path(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
                DEFAULT_EXPORT_FILENAME
            ),
        ];

        let recent = recent_released_records(&state.snapshot.state, RELEASE_HISTORY_LIMIT);
        if recent.is_empty() {
            lines.push("Recent releases: none".to_string());
        } else {
            lines.push("Recent releases:".to_string());
            for entry in recent {
                lines.push(format!(
                    "  {}  {}  {}",
                    entry.port,
                    entry.project_name.unwrap_or_else(|| "unknown".to_string()),
                    relative_time(entry.last_seen_epoch)
                ));
            }
        }

        return lines;
    };

    let history = row.history.as_ref();
    let health = state.health_for(row);
    let state_label = if row.owner.stale { "stale" } else { "active" };
    let tty = row.owner.tty.as_deref().unwrap_or("-");
    let ppid = row
        .owner
        .ppid
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string());
    let first_seen = history
        .map(|entry| relative_time(entry.first_seen_epoch))
        .unwrap_or_else(|| "unknown".to_string());
    let last_seen = history
        .map(|entry| relative_time(entry.last_seen_epoch))
        .unwrap_or_else(|| "unknown".to_string());
    let notes = row
        .owner
        .stale_reason
        .clone()
        .or_else(|| row.owner.cwd.as_ref().map(|path| display_path(path)))
        .unwrap_or_else(|| "listening".to_string());

    vec![
        format!(
            "Port: {}   State: {}   Owners on port: {}",
            row.port, state_label, row.owner_count
        ),
        format!("URL / Health: {} / {}", row.url(), health.details()),
        format!("PID / PPID: {} / {}", row.owner.pid, ppid),
        format!(
            "Age / TTY: {} / {}",
            row.owner
                .age
                .map(format_duration_short)
                .unwrap_or_else(|| "unknown".to_string()),
            tty
        ),
        format!("Project: {}", row.owner.project_name),
        format!(
            "Project root: {}",
            row.owner
                .project_root
                .as_ref()
                .map(|path| display_path(path))
                .unwrap_or_else(|| "unknown".to_string())
        ),
        format!(
            "Working dir: {}",
            row.owner
                .cwd
                .as_ref()
                .map(|path| display_path(path))
                .unwrap_or_else(|| "unknown".to_string())
        ),
        format!("First seen / Last seen: {} / {}", first_seen, last_seen),
        format!("Command: {}", row.owner.command_line),
        format!("Notes: {}", notes),
        "Hints: quick stale|old|restart-old, restart <pid|port> [--port N], filter health:up|tcp|down|unknown, open [pid|port]"
            .to_string(),
    ]
}

fn build_worktree_detail_lines(state: &WorktreeDashboardState) -> Vec<String> {
    let Some(row) = state.selected_row() else {
        return vec![
            "No worktree selected.".to_string(),
            state.repo.as_ref().map_or_else(
                || "Not inside a git repository.".to_string(),
                |repo| format!("Repo root: {}", display_path(&repo.root)),
            ),
            "Hints: Tab -> Worktrees, new feature-x, sync all --from main, task test, sort stale desc"
                .to_string(),
        ];
    };

    let worktree = &row.worktree;
    let upstream = worktree.upstream.as_deref().unwrap_or("-");
    let origin_ref = worktree.origin_ref.as_deref().unwrap_or("-");
    let base_ref = worktree.base_ref.as_deref().unwrap_or("-");
    let git_dir = worktree
        .git_dir
        .as_ref()
        .map(|path| display_path(path))
        .unwrap_or_else(|| "unknown".to_string());
    let last_commit = worktree.last_commit.as_ref().map_or_else(
        || "unknown".to_string(),
        |commit| {
            format!(
                "{} {} {}",
                commit.short_oid,
                relative_time(commit.committed_at_epoch),
                commit.subject
            )
        },
    );
    let mut lines = vec![
        format!("Path: {}", display_path(&worktree.path)),
        format!(
            "Kind / Branch: {} / {}",
            worktree.kind_label(),
            worktree.branch_label()
        ),
        format!(
            "Sync / Flags: {} / +{} -{} / {}",
            upstream,
            worktree.ahead,
            worktree.behind,
            worktree.flag_summary()
        ),
        format!(
            "Origin sync: {} / +{} -{}",
            origin_ref, worktree.origin_ahead, worktree.origin_behind
        ),
        format!(
            "Base sync: {} / +{} -{} / merged:{}",
            base_ref,
            worktree.base_ahead,
            worktree.base_behind,
            match worktree.merged_into_base {
                Some(true) => "yes",
                Some(false) => "no",
                None => "?",
            }
        ),
        format!(
            "Changes: staged:{}  unstaged:{}  untracked:{}  conflicted:{}",
            worktree.staged.len(),
            worktree.unstaged.len(),
            worktree.untracked.len(),
            worktree.conflicted.len()
        ),
        format!(
            "Stale score: {}  Last checkout: {}",
            worktree.stale_score(now_epoch()),
            worktree
                .last_checkout_epoch
                .map(relative_time)
                .unwrap_or_else(|| "unknown".to_string())
        ),
        format!(
            "Preview: {}",
            trim_middle(&worktree_changed_file_preview(worktree, 3), 80)
        ),
    ];

    if worktree.locked {
        lines.push(format!(
            "Lock reason: {}",
            worktree.lock_reason.as_deref().unwrap_or("-")
        ));
    }
    if worktree.prunable {
        lines.push(format!(
            "Prunable: {}",
            worktree.prunable_reason.as_deref().unwrap_or("yes")
        ));
    }

    lines.push("Changed files:".to_string());
    lines.extend(render_file_section("staged", &worktree.staged));
    lines.extend(render_file_section("unstaged", &worktree.unstaged));
    lines.extend(render_file_section("untracked", &worktree.untracked));
    lines.extend(render_file_section("conflicted", &worktree.conflicted));
    lines.push(format!("Git dir: {}", git_dir));
    lines.push(format!("Last commit: {}", trim_middle(&last_commit, 80)));
    if let Some(task) = state
        .last_task
        .as_ref()
        .filter(|task| task.target_path == worktree.path)
    {
        lines.push(format!(
            "Last task: {} {} (exit {}, {}, {})",
            task.preset.label(),
            if task.success { "ok" } else { "failed" },
            task.exit_code,
            format_duration_short(task.duration),
            relative_time(task.finished_at_epoch)
        ));
        lines.push(format!("Task cmd: {}", trim_middle(&task.command, 80)));
        lines.push("Task output:".to_string());
        if task.output_tail.is_empty() {
            lines.push("  (no output)".to_string());
        } else {
            for line in &task.output_tail {
                lines.push(format!("  {}", trim_middle(line, 80)));
            }
        }
    }
    lines.push(
        "Hints: new|open|switch|move|lock|unlock|remove|sync|task|test|lint|build|prune|pick|merge|rebase|reset".to_string(),
    );
    lines
}

fn render_file_section(label: &str, files: &[String]) -> Vec<String> {
    if files.is_empty() {
        return vec![format!("  {}: none", label)];
    }

    let mut lines = vec![format!("  {}:", label)];
    for file in files.iter().take(6) {
        lines.push(format!("    {}", trim_middle(file, 72)));
    }
    if files.len() > 6 {
        lines.push(format!("    ... {} more", files.len() - 6));
    }
    lines
}

fn worktree_changed_file_preview(worktree: &WorktreeRecord, max_items: usize) -> String {
    let mut entries = Vec::new();
    entries.extend(
        worktree
            .conflicted
            .iter()
            .map(|path| format!("conflict:{path}")),
    );
    entries.extend(worktree.staged.iter().map(|path| format!("staged:{path}")));
    entries.extend(
        worktree
            .unstaged
            .iter()
            .map(|path| format!("unstaged:{path}")),
    );
    entries.extend(
        worktree
            .untracked
            .iter()
            .map(|path| format!("untracked:{path}")),
    );

    if entries.is_empty() {
        return "none".to_string();
    }

    let preview = entries
        .iter()
        .take(max_items)
        .map(|item| trim_middle(item, 28))
        .collect::<Vec<_>>()
        .join(", ");
    if entries.len() > max_items {
        format!("{preview} +{}", entries.len() - max_items)
    } else {
        preview
    }
}

fn build_command_menu_lines(view: CockpitView) -> Vec<String> {
    let mut lines = vec![
        "Global".to_string(),
        "  ports / worktrees                jump directly between cockpit views".to_string(),
        "  view ports|worktrees             explicit view switch command".to_string(),
        "  help [ports|worktrees|commands]  open contextual help".to_string(),
        "  refresh                          refresh active view".to_string(),
        "  download [file]                  export ports + worktrees snapshot".to_string(),
        "  yes / no                         confirm or cancel a pending action".to_string(),
        "  quit                             exit cockpit".to_string(),
    ];

    match view {
        CockpitView::Ports => {
            lines.extend([
                "Ports".to_string(),
                "  restart [pid|port] [--port N]   restart selected/target session".to_string(),
                "  quick stale|old|restart-old     run bulk port actions".to_string(),
                "  kill [port|pid]                 terminate selected/target".to_string(),
                "  move <new-port>                 move selected session".to_string(),
                "  filter <expr>                   stale|active|health:*|port:*|project:*|cmd:*"
                    .to_string(),
                "  sort <key> [asc|desc]           port|project|age|state|health".to_string(),
                "  clear                           reset filter + sort".to_string(),
                "  open [port|pid]                 open selected/target URL".to_string(),
                "  select <port|pid>               jump to row".to_string(),
            ]);
        }
        CockpitView::Worktrees => {
            lines.extend([
                "Worktrees".to_string(),
                "  Bare verbs work here: `new`, `switch`, `remove`, `pick`, `merge`, `rebase`"
                    .to_string(),
                "  Prefixes `wt`, `worktree`, and `worktrees` also work if you want explicit scoping"
                    .to_string(),
                "  new <branch> [--from ref]       create branch + sibling worktree path automatically"
                    .to_string(),
                "  add <path> [--branch N] [--from ref] [--detach] [--no-checkout] [--lock]"
                    .to_string(),
                "  open [target] / switch <ref> [target] [--create] [--track]".to_string(),
                "  move <new-path> [target] / lock [target] [--reason txt] / unlock [target]"
                    .to_string(),
                "  remove [target] [--force] / sync [all|target] [--from ref] [--mode rebase|merge]"
                    .to_string(),
                "  sync flags: --include-dirty --include-main / prune [--dry-run] / cleanup [--dry-run]"
                    .to_string(),
                "  pick <commit...> [target] / merge <ref> [target] / rebase <ref> [target]"
                    .to_string(),
                "  reset <--soft|--mixed|--hard> <ref> [target] / continue [target] / abort [target]"
                    .to_string(),
                "  task <test|lint|build> [target]  or bare: test|lint|build [target]".to_string(),
                "  filter <expr>                   dirty|clean|conflicted|locked|prunable|detached|branch:*|path:*"
                    .to_string(),
                "  sort <key> [asc|desc]           path|branch|sync|changes|state|stale".to_string(),
                "  clear                           reset filter + sort".to_string(),
                "  select <target>                 jump by exact path or branch".to_string(),
            ]);
        }
    }

    lines.push("Hotkeys".to_string());
    lines.push("  Tab / Shift+Tab                 switch views".to_string());
    lines.push("  ? or F1                         toggle this menu".to_string());
    lines.push(
        "  Esc                             close menu, clear input, or cancel pending".to_string(),
    );
    lines.push("  Ctrl+C                          quit".to_string());
    lines
}

fn active_list_title(state: &CockpitState) -> String {
    match state.view {
        CockpitView::Ports => format!(
            "Sessions ({}/{})  Tab -> Worktrees",
            state.ports.rows.len(),
            state.ports.all_rows.len()
        ),
        CockpitView::Worktrees => {
            format!(
                "Worktrees ({}/{})  Tab -> Ports",
                state.worktrees.rows.len(),
                state.worktrees.all_rows.len()
            )
        }
    }
}

fn active_list_header(state: &CockpitState, width: usize) -> String {
    match state.view {
        CockpitView::Ports => format_session_header(width),
        CockpitView::Worktrees => format_worktree_header(width),
    }
}

fn build_list_lines(
    state: &CockpitState,
    width: usize,
    list_inner_width: usize,
    list_height: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    match state.view {
        CockpitView::Ports => {
            let start =
                visible_row_start(state.ports.selected, state.ports.rows.len(), list_height);
            for offset in 0..list_height {
                let index = start + offset;
                if let Some(row) = state.ports.rows.get(index) {
                    lines.push(box_line(
                        width,
                        &format_session_row(
                            row,
                            list_inner_width,
                            index == state.ports.selected,
                            state.ports.health_for(row),
                        ),
                    ));
                } else if state.ports.rows.is_empty() && offset == 0 {
                    lines.push(box_line(
                        width,
                        "No rows match current filter. Use `clear`, `filter ...`, or `refresh`.",
                    ));
                } else {
                    lines.push(box_line(width, ""));
                }
            }
        }
        CockpitView::Worktrees => {
            let start = visible_row_start(
                state.worktrees.selected,
                state.worktrees.rows.len(),
                list_height,
            );
            for offset in 0..list_height {
                let index = start + offset;
                if let Some(row) = state.worktrees.rows.get(index) {
                    lines.push(box_line(
                        width,
                        &format_worktree_row(
                            row,
                            list_inner_width,
                            index == state.worktrees.selected,
                        ),
                    ));
                } else if state.worktrees.rows.is_empty() && offset == 0 {
                    let message = if state.worktrees.repo.is_none() {
                        "No git repository found from the current directory."
                    } else {
                        "No worktrees match current filter. Use `clear`, `filter ...`, or `refresh`."
                    };
                    lines.push(box_line(width, message));
                } else {
                    lines.push(box_line(width, ""));
                }
            }
        }
    }
    lines
}

fn format_session_header(width: usize) -> String {
    let prefix = format!(
        "{} {:>5} {:<6} {:<7} {:>5} {:>6} {:>6} {:<8} ",
        " ", "Port", "State", "Health", "Age", "PID", "PPID", "TTY"
    );
    let remaining = width.saturating_sub(prefix.chars().count());
    format!("{}{}", prefix, truncate_end("Project  URL", remaining))
}

fn format_session_row(
    row: &DashboardRow,
    width: usize,
    selected: bool,
    health: HealthStatus,
) -> String {
    let marker = if selected { ">" } else { " " };
    let state = if row.owner.stale { "STALE" } else { "active" };
    let age = row
        .owner
        .age
        .map(format_duration_short)
        .unwrap_or_else(|| "?".to_string());
    let ppid = row
        .owner
        .ppid
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string());
    let tty = row.owner.tty.as_deref().unwrap_or("-");
    let prefix = format!(
        "{} {:>5} {:<6} {:<7} {:>5} {:>6} {:>6} {:<8} ",
        marker,
        row.port,
        state,
        health.badge(),
        age,
        row.owner.pid,
        ppid,
        tty
    );
    let suffix = format!(
        "{}  {}",
        truncate_end(&row.owner.project_name, 16),
        truncate_end(&row.url(), 22)
    );
    let remaining = width.saturating_sub(prefix.chars().count());
    format!("{}{}", prefix, truncate_end(&suffix, remaining))
}

fn format_worktree_header(width: usize) -> String {
    let prefix = format!(
        "{} {:<8} {:<18} {:<9} {:<10} ",
        " ", "Kind", "Branch", "Sync", "Changes"
    );
    let remaining = width.saturating_sub(prefix.chars().count());
    format!(
        "{}{}",
        prefix,
        truncate_end("State  Stale  Files  Path", remaining)
    )
}

fn format_worktree_row(row: &WorktreeRow, width: usize, selected: bool) -> String {
    let marker = if selected { ">" } else { " " };
    let sync = if row.worktree.upstream.is_some() {
        format!("+{} -{}", row.worktree.ahead, row.worktree.behind)
    } else {
        "-".to_string()
    };
    let changes = format!(
        "s{} u{} ?{} c{}",
        row.worktree.staged.len(),
        row.worktree.unstaged.len(),
        row.worktree.untracked.len(),
        row.worktree.conflicted.len()
    );
    let prefix = format!(
        "{} {:<8} {:<18} {:<9} {:<10} ",
        marker,
        row.worktree.kind_label(),
        truncate_end(&row.worktree.branch_label(), 18),
        truncate_end(&sync, 9),
        truncate_end(&changes, 10),
    );
    let suffix = format!(
        "{}  {:>5}  {}  {}",
        truncate_end(&row.worktree.flag_summary(), 18),
        row.worktree.stale_score(now_epoch()),
        truncate_end(&worktree_changed_file_preview(&row.worktree, 2), 28),
        truncate_end(&display_path(&row.worktree.path), 30)
    );
    let remaining = width.saturating_sub(prefix.chars().count());
    format!("{}{}", prefix, truncate_end(&suffix, remaining))
}

fn visible_row_start(selected: usize, total: usize, height: usize) -> usize {
    if total <= height || height == 0 {
        return 0;
    }

    let half = height / 2;
    let mut start = selected.saturating_sub(half);
    if start + height > total {
        start = total.saturating_sub(height);
    }
    start
}

fn dashboard_layout(height: usize, has_rows: bool) -> (usize, usize) {
    // Non-resizable lines:
    // 5 overview + 3 session framing + 2 detail framing + 3 footer/prompt lines.
    let fixed_lines = 13usize;
    let body_lines = height.saturating_sub(fixed_lines);
    let mut list_height = body_lines.saturating_mul(3) / 5;
    let mut detail_height = body_lines.saturating_sub(list_height);
    if has_rows && list_height < 4 {
        let desired = 4usize.saturating_sub(list_height);
        let shift = desired.min(detail_height.saturating_sub(3));
        list_height += shift;
        detail_height = body_lines.saturating_sub(list_height);
    }
    (list_height, detail_height)
}

fn probe_visible_rows(state: &mut DashboardState, timeout: Duration) -> Result<()> {
    let (_, height) = terminal::size().context("failed to read terminal size")?;
    let (list_height, _) = dashboard_layout(height as usize, !state.rows.is_empty());
    let start = visible_row_start(state.selected, state.rows.len(), list_height);
    let end = (start + list_height).min(state.rows.len());

    let keys: Vec<_> = state.rows[start..end]
        .iter()
        .map(DashboardRow::key)
        .collect();
    probe_rows_by_keys(state, &keys, timeout);
    Ok(())
}

fn probe_rows_by_keys(state: &mut DashboardState, keys: &[(u16, i32)], timeout: Duration) {
    for (port, pid) in keys {
        let health = probe_port_health(*port, timeout);
        state.apply_health_sample(*port, *pid, health);
    }
}

fn truncate_end(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if value.chars().count() <= max {
        return value.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let prefix: String = value.chars().take(max.saturating_sub(1)).collect();
    format!("{prefix}…")
}

fn visible_input_tail(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if value.chars().count() <= max {
        return value.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let suffix: String = value
        .chars()
        .rev()
        .take(max.saturating_sub(1))
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("…{suffix}")
}

fn resolve_export_path(path: Option<&str>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    Ok(resolve_export_path_from(&cwd, path))
}

fn resolve_export_path_from(cwd: &Path, path: Option<&str>) -> PathBuf {
    let path = path
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.join(DEFAULT_EXPORT_FILENAME));

    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn export_dashboard_snapshot(
    snapshot: &Snapshot,
    repo: Option<&RepoSnapshot>,
    stale_after: Duration,
    path: &Path,
) -> Result<()> {
    let summary = build_dashboard_summary(snapshot);
    let free_ports = free_ports(
        DEFAULT_AVAILABLE_FROM,
        DEFAULT_AVAILABLE_TO,
        &snapshot.active,
        12,
    );
    let recent_released = recent_released_records(&snapshot.state, RELEASE_HISTORY_LIMIT)
        .into_iter()
        .map(|entry| DashboardExportReleased {
            port: entry.port,
            project_name: entry.project_name,
            project_root: entry.project_root,
            command_line: entry.command_line,
            pid: entry.pid,
            first_seen_epoch: entry.first_seen_epoch,
            last_seen_epoch: entry.last_seen_epoch,
            released_at_epoch: entry.released_at_epoch,
        })
        .collect();

    let active = snapshot
        .active
        .iter()
        .map(|entry| {
            let history = snapshot.state.ports.get(&entry.port);
            DashboardExportPort {
                port: entry.port,
                owner_count: entry.owners.len(),
                first_seen_epoch: history.map(|item| item.first_seen_epoch),
                last_seen_epoch: history.map(|item| item.last_seen_epoch),
                owners: entry
                    .owners
                    .iter()
                    .map(|owner| DashboardExportOwner {
                        pid: owner.pid,
                        ppid: owner.ppid,
                        tty: owner.tty.clone(),
                        command_line: owner.command_line.clone(),
                        cwd: owner.cwd.clone(),
                        project_root: owner.project_root.clone(),
                        project_name: owner.project_name.clone(),
                        age_seconds: owner.age.map(|age| age.as_secs()),
                        stale: owner.stale,
                        stale_reason: owner.stale_reason.clone(),
                    })
                    .collect(),
            }
        })
        .collect();

    let repo_export = build_worktree_summary(repo);
    let export_now = now_epoch();
    let worktrees_export = repo.map(|repo| {
        repo.worktrees
            .iter()
            .map(|worktree| DashboardExportWorktree {
                path: worktree.path.clone(),
                is_main: worktree.is_main,
                branch: worktree.branch.clone(),
                detached: worktree.detached,
                locked: worktree.locked,
                lock_reason: worktree.lock_reason.clone(),
                prunable: worktree.prunable,
                prunable_reason: worktree.prunable_reason.clone(),
                upstream: worktree.upstream.clone(),
                ahead: worktree.ahead,
                behind: worktree.behind,
                origin_ref: worktree.origin_ref.clone(),
                origin_ahead: worktree.origin_ahead,
                origin_behind: worktree.origin_behind,
                base_ref: worktree.base_ref.clone(),
                base_ahead: worktree.base_ahead,
                base_behind: worktree.base_behind,
                merged_into_base: worktree.merged_into_base,
                head_oid: worktree.head_oid.clone(),
                git_dir: worktree.git_dir.clone(),
                operations: worktree
                    .operations
                    .iter()
                    .map(|operation| operation.label().to_string())
                    .collect(),
                staged: worktree.staged.clone(),
                unstaged: worktree.unstaged.clone(),
                untracked: worktree.untracked.clone(),
                conflicted: worktree.conflicted.clone(),
                last_commit_epoch: worktree
                    .last_commit
                    .as_ref()
                    .map(|commit| commit.committed_at_epoch),
                last_commit_subject: worktree
                    .last_commit
                    .as_ref()
                    .map(|commit| commit.subject.clone()),
                last_checkout_epoch: worktree.last_checkout_epoch,
                stale_score: worktree.stale_score(export_now),
            })
            .collect::<Vec<_>>()
    });

    let export = DashboardExport {
        exported_at_epoch: now_epoch(),
        cwd: std::env::current_dir().context("failed to determine current directory")?,
        stale_after_seconds: stale_after.as_secs(),
        summary,
        free_ports,
        active,
        recent_released,
        repo: repo_export,
        worktrees: worktrees_export,
    };

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
    }

    let json = serde_json::to_string_pretty(&export)?;
    fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn parse_port_token(value: &str) -> Result<u16> {
    value
        .parse::<u16>()
        .with_context(|| format!("invalid port `{}`", value))
}

fn parse_pid_token(value: &str) -> Result<i32> {
    value
        .parse::<i32>()
        .with_context(|| format!("invalid pid `{}`", value))
}

fn print_active_table(entries: &[PortSnapshot]) {
    let mut table = build_table();
    table.set_header(vec![
        "Port", "State", "PID", "PPID", "TTY", "Project", "Age", "Command", "Notes",
    ]);

    for entry in entries {
        for owner in &entry.owners {
            let state = if owner.stale {
                Cell::new("stale").fg(Color::Yellow)
            } else {
                Cell::new("active").fg(Color::Green)
            };

            let notes = owner
                .stale_reason
                .clone()
                .or_else(|| owner.cwd.as_ref().map(|path| display_path(path)))
                .unwrap_or_else(|| "listening".to_string());

            table.add_row(vec![
                Cell::new(entry.port).set_alignment(CellAlignment::Right),
                state,
                Cell::new(owner.pid).set_alignment(CellAlignment::Right),
                Cell::new(
                    owner
                        .ppid
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                )
                .set_alignment(CellAlignment::Right),
                Cell::new(owner.tty.clone().unwrap_or_else(|| "-".to_string())),
                Cell::new(&owner.project_name),
                Cell::new(
                    owner
                        .age
                        .map(format_duration_short)
                        .unwrap_or_else(|| "unknown".to_string()),
                ),
                Cell::new(trim_middle(&owner.command_line, 52)),
                Cell::new(trim_middle(&notes, 44)),
            ]);
        }
    }

    println!("{table}");
}

fn print_released_table(entries: &[PersistedPortRecord]) {
    let mut table = build_table();
    table.set_header(vec!["Port", "Project", "Last seen", "Command"]);

    for entry in entries {
        table.add_row(vec![
            Cell::new(entry.port).set_alignment(CellAlignment::Right),
            Cell::new(
                entry
                    .project_name
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
            Cell::new(relative_time(entry.last_seen_epoch)),
            Cell::new(trim_middle(
                entry.command_line.as_deref().unwrap_or("unknown"),
                72,
            )),
        ]);
    }

    println!("{table}");
}

fn build_table() -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
}

fn spawn_and_capture(command: &[String]) -> Result<CommandOutcome> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("command is empty"))?;

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {:?}", command))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("child stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("child stderr unavailable"))?;

    let stdout_capture = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let stderr_capture = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let stdout_handle = pipe_stream(stdout, io::stdout(), stdout_capture.clone());
    let stderr_handle = pipe_stream(stderr, io::stderr(), stderr_capture.clone());

    let status = child.wait()?;
    stdout_handle
        .join()
        .map_err(|_| anyhow!("stdout forwarding thread panicked"))??;
    stderr_handle
        .join()
        .map_err(|_| anyhow!("stderr forwarding thread panicked"))??;

    let stderr = String::from_utf8_lossy(&stderr_capture.lock().unwrap()).to_string();
    Ok(CommandOutcome { status, stderr })
}

fn pipe_stream<R, W>(
    mut reader: R,
    mut writer: W,
    capture: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
) -> thread::JoinHandle<Result<()>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            let bytes = reader.read(&mut buffer)?;
            if bytes == 0 {
                break;
            }
            writer.write_all(&buffer[..bytes])?;
            writer.flush()?;

            let mut captured = capture.lock().unwrap();
            append_limited(&mut captured, &buffer[..bytes], CAPTURE_LIMIT);
        }
        Ok(())
    })
}

fn append_limited(buffer: &mut Vec<u8>, chunk: &[u8], limit: usize) {
    buffer.extend_from_slice(chunk);
    if buffer.len() > limit {
        let overflow = buffer.len() - limit;
        buffer.drain(..overflow);
    }
}

fn detect_conflicting_port(stderr: &str) -> Option<u16> {
    for line in stderr.lines().rev() {
        let normalized = line.to_ascii_lowercase();
        if !normalized.contains("in use") && !normalized.contains("eaddrinuse") {
            continue;
        }

        if let Some(captures) = PORT_FROM_ERROR_PATTERN.captures(line) {
            for name in ["label", "suffix", "trailing"] {
                if let Some(value) = captures.name(name) {
                    if let Ok(port) = value.as_str().parse::<u16>() {
                        return Some(port);
                    }
                }
            }
        }

        if let Some(port) = extract_port(line) {
            return Some(port);
        }
    }

    None
}

fn terminate_processes(pids: &[i32]) -> Result<()> {
    for pid in pids {
        let status = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .with_context(|| format!("failed to send SIGTERM to pid {}", pid))?;
        if !status.success() {
            bail!("kill -TERM {} failed", pid);
        }
    }
    Ok(())
}

fn wait_for_port_to_clear(
    port: u16,
    timeout: Duration,
    stale_after: Duration,
) -> Result<Option<PortSnapshot>> {
    let deadline = SystemTime::now() + timeout;
    loop {
        let active = discover_active_ports(stale_after)?;
        if let Some(snapshot) = active.into_iter().find(|entry| entry.port == port) {
            if SystemTime::now() >= deadline {
                return Ok(Some(snapshot));
            }
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        return Ok(None);
    }
}

fn unique_pids(active: &PortSnapshot) -> Vec<i32> {
    let mut pids = BTreeSet::new();
    pids.extend(active.owners.iter().map(|owner| owner.pid));
    pids.into_iter().collect()
}

fn describe_pids(pids: &[i32]) -> String {
    match pids {
        [pid] => format!("pid {}", pid),
        _ => format!(
            "pids {}",
            pids.iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(false);
    }

    Confirm::new()
        .with_prompt(prompt)
        .default(true)
        .interact()
        .context("failed to read interactive confirmation")
}

fn parse_duration(input: &str) -> Result<Duration, String> {
    humantime::parse_duration(input).map_err(|error| error.to_string())
}

fn extract_port(value: &str) -> Option<u16> {
    let digits: String = value
        .chars()
        .rev()
        .skip_while(|ch| !ch.is_ascii_digit())
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    if digits.is_empty() {
        None
    } else {
        digits.parse::<u16>().ok()
    }
}

fn parse_elapsed_time(value: &str) -> Option<Duration> {
    let (days, rest) = if let Some((days, rest)) = value.split_once('-') {
        (days.parse::<u64>().ok()?, rest)
    } else {
        (0, value)
    };

    let parts: Vec<_> = rest.split(':').collect();
    let seconds = match parts.as_slice() {
        [minutes, seconds] => minutes.parse::<u64>().ok()? * 60 + seconds.parse::<u64>().ok()?,
        [hours, minutes, seconds] => {
            hours.parse::<u64>().ok()? * 3600
                + minutes.parse::<u64>().ok()? * 60
                + seconds.parse::<u64>().ok()?
        }
        _ => return None,
    };

    Some(Duration::from_secs(days * 86_400 + seconds))
}

fn relative_time(epoch: i64) -> String {
    let now = now_epoch();
    if epoch <= 0 || epoch > now {
        return "just now".to_string();
    }

    let delta = Duration::from_secs((now - epoch) as u64);
    format!("{} ago", format_duration_short(delta))
}

fn format_duration_short(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        return format!("{}s", seconds);
    }
    if seconds < 3600 {
        return format!("{}m", seconds / 60);
    }
    if seconds < 86_400 {
        return format!("{}h", seconds / 3600);
    }
    format!("{}d", seconds / 86_400)
}

fn display_path(path: &Path) -> String {
    let home = dirs::home_dir();
    if let Some(home) = home {
        if let Ok(stripped) = path.strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }

    path.display().to_string()
}

fn trim_middle(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }

    let left = max / 2;
    let right = max.saturating_sub(left + 1);
    let prefix: String = value.chars().take(left).collect();
    let suffix: String = value
        .chars()
        .rev()
        .take(right)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn exit_code_from_status(status: ExitStatus) -> ExitCode {
    let code = status.code().unwrap_or(1);
    ExitCode::from(code.clamp(0, 255) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;
    use worktree::discover_repo_from;

    #[test]
    fn parses_lsof_output() {
        let input = "p4128\ncnode\nn*:3000\nn127.0.0.1:3000\np5120\ncpython\nn127.0.0.1:8080\n";
        let listeners = parse_lsof_output(input);
        assert_eq!(listeners.len(), 3);
        assert_eq!(listeners[0].pid, 4128);
        assert_eq!(listeners[0].port, 3000);
        assert_eq!(listeners[2].command_name, "python");
    }

    #[test]
    fn parses_ps_line() {
        let line = "4128     1 ??         1-03:12:02 node /tmp/dev-server.js";
        let parsed = parse_ps_line(line).expect("ps line parsed");
        assert_eq!(parsed.0, 4128);
        assert_eq!(parsed.1, Some(1));
        assert_eq!(parsed.2.as_deref(), Some("??"));
        assert_eq!(parsed.4, "node /tmp/dev-server.js");
        assert_eq!(parsed.3.unwrap().as_secs(), 97_922);
    }

    #[test]
    fn detects_port_from_bind_error() {
        let stderr = "Error: listen EADDRINUSE: address already in use :::3000\n";
        assert_eq!(detect_conflicting_port(stderr), Some(3000));
    }

    #[test]
    fn flags_orphaned_dev_server_as_stale() {
        let meta = ProcessMeta {
            ppid: Some(1),
            tty: Some("??".to_string()),
            age: Some(Duration::from_secs(4_000)),
            command_line: "node next dev".to_string(),
            cwd: None,
        };
        assert_eq!(
            determine_stale_reason(&meta, Duration::from_secs(300)).as_deref(),
            Some("orphaned dev server without a terminal")
        );
    }

    #[test]
    fn finds_project_root_from_marker() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("app");
        let nested = root.join("src/pages");
        fs::create_dir_all(&nested).expect("dirs");
        fs::write(root.join("package.json"), "{}").expect("marker");

        let project_root = guess_project_root(&nested).expect("project root");
        assert_eq!(project_root, root);
    }

    #[test]
    fn parses_dashboard_move_command() {
        let command = parse_dashboard_command("move 3000 3100").expect("command");
        assert_eq!(
            command,
            DashboardCommand::Move {
                from: Some(3000),
                to: 3100,
            }
        );
    }

    #[test]
    fn export_path_defaults_to_current_root() {
        let temp = tempdir().expect("tempdir");
        let path = resolve_export_path_from(temp.path(), None);
        assert_eq!(path, temp.path().join(DEFAULT_EXPORT_FILENAME));
    }

    #[test]
    fn parses_restart_command_with_override() {
        let command = parse_dashboard_command("restart 3000 --port 3010").expect("command");
        assert_eq!(
            command,
            DashboardCommand::Restart(RestartRequest {
                target: Some("3000".to_string()),
                port_override: Some(3010),
            })
        );
    }

    #[test]
    fn parses_sort_command_with_direction() {
        let command = parse_dashboard_command("sort health desc").expect("command");
        assert_eq!(
            command,
            DashboardCommand::Sort(DashboardSort {
                key: DashboardSortKey::Health,
                direction: SortDirection::Desc,
            })
        );
    }

    #[test]
    fn parses_filter_and_open_commands() {
        let filter = parse_dashboard_command("filter health:up").expect("filter");
        assert_eq!(filter, DashboardCommand::Filter("health:up".to_string()));

        let open = parse_dashboard_command("open 3000").expect("open");
        assert_eq!(open, DashboardCommand::Open(Some("3000".to_string())));
    }

    #[test]
    fn parses_dashboard_filter_expressions() {
        match DashboardFilter::parse("project:web").expect("project filter") {
            DashboardFilter::ProjectContains(value) => assert_eq!(value, "web"),
            other => panic!("expected project filter, got {:?}", other),
        }

        match DashboardFilter::parse("health:tcp").expect("health filter") {
            DashboardFilter::Health(HealthFilter::Tcp) => {}
            other => panic!("expected tcp health filter, got {:?}", other),
        }
    }

    #[test]
    fn parses_quick_action_command() {
        let command = parse_dashboard_command("quick stale").expect("command");
        assert_eq!(command, DashboardCommand::Quick(QuickAction::KillStale));
    }

    #[test]
    fn parses_sort_new_old_shorthand() {
        let command = parse_dashboard_command("sort new-old").expect("command");
        assert_eq!(
            command,
            DashboardCommand::Sort(DashboardSort {
                key: DashboardSortKey::Age,
                direction: SortDirection::Asc,
            })
        );
    }

    #[test]
    fn parses_worktree_add_and_reset_commands() {
        let empty = WorktreeDashboardState::new(None);

        match parse_worktree_command(
            "add ../feature --branch feat --from main --lock --no-checkout",
            &empty,
        )
        .expect("add")
        {
            WorktreeCommand::Add(request) => {
                assert_eq!(request.path, "../feature");
                assert_eq!(request.branch.as_deref(), Some("feat"));
                assert_eq!(request.from_ref.as_deref(), Some("main"));
                assert!(request.lock);
                assert!(request.no_checkout);
            }
            other => panic!("expected add, got {:?}", other),
        }

        match parse_worktree_command("reset --hard HEAD~1", &empty).expect("reset") {
            WorktreeCommand::Reset(request) => {
                assert_eq!(request.mode, ResetMode::Hard);
                assert_eq!(request.reference, "HEAD~1");
                assert!(request.target.is_none());
            }
            other => panic!("expected reset, got {:?}", other),
        }

        match parse_worktree_command("create ../feature-2", &empty).expect("create") {
            WorktreeCommand::Add(request) => {
                assert_eq!(request.path, "../feature-2");
                assert!(!request.implicit_path_from_branch);
            }
            other => panic!("expected add alias, got {:?}", other),
        }

        match parse_worktree_command("new feature/login --from main", &empty).expect("new") {
            WorktreeCommand::Add(request) => {
                assert_eq!(request.branch.as_deref(), Some("feature/login"));
                assert_eq!(request.path, "../feature-login");
                assert_eq!(request.from_ref.as_deref(), Some("main"));
                assert!(request.implicit_path_from_branch);
            }
            other => panic!("expected add shorthand, got {:?}", other),
        }

        match parse_worktree_command("delete --force", &empty).expect("delete") {
            WorktreeCommand::Remove(request) => {
                assert!(request.force);
                assert!(request.target.is_none());
            }
            other => panic!("expected remove alias, got {:?}", other),
        }

        match parse_worktree_command(
            "sync all --from main --mode merge --include-dirty --include-main",
            &empty,
        )
        .expect("sync")
        {
            WorktreeCommand::Sync(request) => {
                assert!(request.all);
                assert_eq!(request.from_ref.as_deref(), Some("main"));
                assert_eq!(request.mode, WorktreeSyncMode::Merge);
                assert!(request.include_dirty);
                assert!(request.include_main);
            }
            other => panic!("expected sync, got {:?}", other),
        }

        match parse_worktree_command("task lint", &empty).expect("task") {
            WorktreeCommand::Task(request) => {
                assert_eq!(request.preset, WorktreeTaskPreset::Lint);
                assert!(request.target.is_none());
            }
            other => panic!("expected task, got {:?}", other),
        }

        match parse_worktree_command("test feature-x", &empty).expect("test alias") {
            WorktreeCommand::Task(request) => {
                assert_eq!(request.preset, WorktreeTaskPreset::Test);
                assert_eq!(request.target.as_deref(), Some("feature-x"));
            }
            other => panic!("expected task alias, got {:?}", other),
        }
    }

    #[test]
    fn parses_worktree_stale_sort_key() {
        let sort = parse_worktree_sort("stale desc").expect("stale sort");
        assert_eq!(sort.key, WorktreeSortKey::Stale);
        assert_eq!(sort.direction, SortDirection::Desc);
    }

    #[test]
    fn parses_cherry_pick_target_from_worktree_state() {
        let root = PathBuf::from("/tmp/repo");
        let feature_path = root.join("feature");
        let worktree = WorktreeRecord {
            path: feature_path.clone(),
            is_main: false,
            bare: false,
            head_oid: "abc".to_string(),
            branch: Some("feature".to_string()),
            detached: false,
            locked: false,
            lock_reason: None,
            prunable: false,
            prunable_reason: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            origin_ref: None,
            origin_ahead: 0,
            origin_behind: 0,
            base_ref: Some("main".to_string()),
            base_ahead: 0,
            base_behind: 0,
            merged_into_base: Some(false),
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: Vec::new(),
            conflicted: Vec::new(),
            operations: Vec::new(),
            last_commit: None,
            last_checkout_epoch: None,
            git_dir: None,
        };
        let state = WorktreeDashboardState {
            repo: Some(RepoSnapshot {
                root: root.clone(),
                common_dir: root.join(".git"),
                base_ref: Some("main".to_string()),
                worktrees: vec![worktree.clone()],
            }),
            rows: vec![WorktreeRow {
                worktree: worktree.clone(),
            }],
            all_rows: vec![WorktreeRow { worktree }],
            selected: 0,
            filter: None,
            sort: WorktreeSort::default(),
            message: DashboardMessage::info("test"),
            last_task: None,
        };

        match parse_worktree_command("cherry-pick abc123 feature", &state).expect("command") {
            WorktreeCommand::CherryPick(request) => {
                assert_eq!(request.commits, vec!["abc123".to_string()]);
                assert_eq!(request.target.as_deref(), Some("feature"));
            }
            other => panic!("expected cherry-pick, got {:?}", other),
        }
    }

    #[test]
    fn discovers_linked_worktrees_and_change_sets() {
        let temp = tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let feature = temp.path().join("feature-tree");
        fs::create_dir_all(&repo).expect("repo dir");

        git(&repo, &["init", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "Port Map"]);
        git(&repo, &["config", "user.email", "portmap@example.com"]);
        fs::write(repo.join("tracked.txt"), "one\n").expect("tracked file");
        git(&repo, &["add", "tracked.txt"]);
        git(&repo, &["commit", "-m", "init"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                feature.to_str().expect("path"),
            ],
        );

        fs::write(repo.join("tracked.txt"), "one\nmain dirty\n").expect("main dirty");
        fs::write(feature.join("feature.txt"), "feature staged\n").expect("feature file");
        git(&feature, &["add", "feature.txt"]);
        fs::write(feature.join("scratch.txt"), "feature untracked\n").expect("untracked");

        let discovered = discover_repo_from(&repo)
            .expect("discover repo")
            .expect("repo exists");
        assert_eq!(discovered.worktrees.len(), 2);
        let feature = fs::canonicalize(feature).expect("canonical feature");

        let main = discovered
            .worktrees
            .iter()
            .find(|item| item.is_main)
            .expect("main worktree");
        assert!(main.unstaged.iter().any(|path| path == "tracked.txt"));

        let linked = discovered
            .worktrees
            .iter()
            .find(|item| item.path == feature)
            .expect("linked worktree");
        assert_eq!(linked.branch.as_deref(), Some("feature"));
        assert!(linked.staged.iter().any(|path| path == "feature.txt"));
        assert!(linked.untracked.iter().any(|path| path == "scratch.txt"));
    }

    #[test]
    fn changed_file_preview_includes_specific_paths() {
        let worktree = WorktreeRecord {
            path: PathBuf::from("/tmp/repo"),
            is_main: true,
            bare: false,
            head_oid: "abc".to_string(),
            branch: Some("main".to_string()),
            detached: false,
            locked: false,
            lock_reason: None,
            prunable: false,
            prunable_reason: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            origin_ref: None,
            origin_ahead: 0,
            origin_behind: 0,
            base_ref: Some("main".to_string()),
            base_ahead: 0,
            base_behind: 0,
            merged_into_base: Some(true),
            staged: vec!["src/main.rs".to_string()],
            unstaged: vec!["README.md".to_string()],
            untracked: vec!["notes.txt".to_string()],
            conflicted: Vec::new(),
            operations: Vec::new(),
            last_commit: None,
            last_checkout_epoch: None,
            git_dir: None,
        };

        let preview = worktree_changed_file_preview(&worktree, 2);
        assert!(preview.contains("staged:src/main.rs"));
        assert!(preview.contains("unstaged:README.md"));
        assert!(preview.ends_with("+1"));
    }

    #[test]
    fn strips_prefixed_worktree_commands_and_detects_bare_aliases() {
        assert_eq!(
            strip_worktree_command_prefix("worktree add ../feature"),
            Some("add ../feature")
        );
        assert_eq!(
            strip_worktree_command_prefix("worktrees remove --force"),
            Some("remove --force")
        );
        assert_eq!(
            strip_worktree_command_prefix("wt switch main"),
            Some("switch main")
        );

        assert!(is_bare_worktree_command("create ../feature"));
        assert!(is_bare_worktree_command("pick abc123"));
        assert!(is_bare_worktree_command("cleanup --dry-run"));
        assert!(is_bare_worktree_command("sync all --from main"));
        assert!(is_bare_worktree_command("task test"));
        assert!(is_bare_worktree_command("build"));
        assert!(!is_bare_worktree_command("open"));
    }

    #[test]
    fn identifies_remove_safety_risks() {
        let worktree = WorktreeRecord {
            path: PathBuf::from("/tmp/repo-feature"),
            is_main: false,
            bare: false,
            head_oid: "abc".to_string(),
            branch: Some("feature".to_string()),
            detached: false,
            locked: true,
            lock_reason: None,
            prunable: false,
            prunable_reason: None,
            upstream: Some("origin/feature".to_string()),
            ahead: 2,
            behind: 0,
            origin_ref: Some("origin/feature".to_string()),
            origin_ahead: 2,
            origin_behind: 0,
            base_ref: Some("main".to_string()),
            base_ahead: 5,
            base_behind: 0,
            merged_into_base: Some(false),
            staged: vec!["src/main.rs".to_string()],
            unstaged: Vec::new(),
            untracked: Vec::new(),
            conflicted: Vec::new(),
            operations: vec![WorktreeOperation::Rebase],
            last_commit: None,
            last_checkout_epoch: None,
            git_dir: None,
        };

        let risks = worktree_remove_safety_risks(&worktree);
        assert!(risks.iter().any(|risk| risk == "locked"));
        assert!(risks.iter().any(|risk| risk.starts_with("dirty:")));
        assert!(risks.iter().any(|risk| risk.starts_with("in-progress:")));
        assert!(risks.iter().any(|risk| risk == "unpushed:2"));
        assert!(risks.iter().any(|risk| risk == "unmerged:main"));
    }

    #[test]
    fn sanitizes_branch_name_for_default_new_path() {
        assert_eq!(
            default_new_worktree_path("feature/login"),
            "../feature-login"
        );
        assert_eq!(default_new_worktree_path("fix.issue_42"), "../fix.issue_42");
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
