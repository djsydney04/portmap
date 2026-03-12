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

const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(30 * 60);
const DEFAULT_DASHBOARD_REFRESH: Duration = Duration::from_secs(3);
const DEFAULT_AVAILABLE_FROM: u16 = 3000;
const DEFAULT_AVAILABLE_TO: u16 = 3999;
const DEFAULT_AVAILABLE_COUNT: usize = 12;
const RELEASE_HISTORY_LIMIT: usize = 8;
const CAPTURE_LIMIT: usize = 64 * 1024;
const DEFAULT_EXPORT_FILENAME: &str = "portmap-dashboard.json";
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
    name = "portmap",
    version,
    about = "Persistent visual map of what's running on which ports"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show the current port map
    Map(MapArgs),
    /// Inspect one port and show its current or last known owner
    Status(StatusArgs),
    /// Find free ports in a range
    Available(AvailableArgs),
    /// Kill the process currently listening on a port
    Release(ReleaseArgs),
    /// Run a command, detect port conflicts, and offer stale-port cleanup
    Run(RunArgs),
    /// Print shell helpers for wrapping dev commands with portmap
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
  command portmap run -- "$@"
}}

pmport() {{
  local port="$1"
  shift
  command portmap run --port "$port" -- "$@"
}}"#
            );
        }
        Shell::Fish => {
            println!(
                r#"function pmrun
  command portmap run -- $argv
end

function pmport
  set -l port $argv[1]
  set -e argv[1]
  command portmap run --port $port -- $argv
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
        "Portmap left port {} alone because it does not look stale. Use `portmap release {}` if you want to stop it manually.",
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
    input: String,
    message: DashboardMessage,
    filter: Option<DashboardFilter>,
    sort: DashboardSort,
    health_cache: HashMap<(u16, i32), HealthStatus>,
    show_command_menu: bool,
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

#[derive(Debug, Clone, Serialize)]
struct DashboardExport {
    exported_at_epoch: i64,
    cwd: PathBuf,
    stale_after_seconds: u64,
    summary: DashboardExportSummary,
    free_ports: Vec<u16>,
    active: Vec<DashboardExportPort>,
    recent_released: Vec<DashboardExportReleased>,
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

fn parse_ps_line(
    line: &str,
) -> Option<(i32, Option<i32>, Option<String>, Option<Duration>, String)> {
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
    Ok(base.join("portmap").join("state.json"))
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
            input: String::new(),
            message: DashboardMessage::info(
                "Use ? or F1 for command menu. Use Up/Down to inspect sessions. Ctrl+C quits.",
            ),
            filter: None,
            sort: DashboardSort::default(),
            health_cache: HashMap::new(),
            show_command_menu: false,
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

impl DashboardRow {
    fn key(&self) -> (u16, i32) {
        (self.port, self.owner.pid)
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
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
    let mut state = DashboardState::new(snapshot);
    probe_visible_rows(&mut state, Duration::from_millis(180))?;
    state.rebuild_rows(None);
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
            match refresh_snapshot(args.stale_after) {
                Ok(snapshot) => {
                    state.apply_snapshot(snapshot);
                    if let Err(error) = probe_visible_rows(&mut state, Duration::from_millis(180)) {
                        state.message =
                            DashboardMessage::error(format!("health probe failed: {error:#}"));
                    } else {
                        state.rebuild_rows(None);
                    }
                }
                Err(error) => {
                    state.message = DashboardMessage::error(format!("refresh failed: {error:#}"));
                }
            }
            last_refresh = Instant::now();
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn handle_dashboard_event(
    state: &mut DashboardState,
    event: Event,
    args: &MapArgs,
) -> Result<bool> {
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
        KeyCode::F(1) => {
            state.show_command_menu = !state.show_command_menu;
            state.message = DashboardMessage::info(if state.show_command_menu {
                "Opened command menu."
            } else {
                "Closed command menu."
            });
        }
        KeyCode::Up => {
            state.selected = state.selected.saturating_sub(1);
        }
        KeyCode::Down => {
            if !state.rows.is_empty() {
                state.selected = (state.selected + 1).min(state.rows.len().saturating_sub(1));
            }
        }
        KeyCode::PageUp => {
            state.selected = state.selected.saturating_sub(5);
        }
        KeyCode::PageDown => {
            if !state.rows.is_empty() {
                state.selected = (state.selected + 5).min(state.rows.len().saturating_sub(1));
            }
        }
        KeyCode::Home => {
            state.selected = 0;
        }
        KeyCode::End => {
            if !state.rows.is_empty() {
                state.selected = state.rows.len().saturating_sub(1);
            }
        }
        KeyCode::Esc => {
            if state.show_command_menu {
                state.show_command_menu = false;
                state.message = DashboardMessage::info("Closed command menu.");
            } else {
                state.input.clear();
                state.message = DashboardMessage::info("Cleared command input.");
            }
        }
        KeyCode::Backspace => {
            state.input.pop();
        }
        KeyCode::Enter => {
            let input = std::mem::take(&mut state.input);
            if input.trim().is_empty() {
                state.message =
                    DashboardMessage::info("Enter a command or use Up/Down to inspect sessions.");
                return Ok(false);
            }

            if execute_dashboard_command(&input, state, args)? {
                return Ok(true);
            }
        }
        KeyCode::Char('?') if state.input.is_empty() => {
            state.show_command_menu = !state.show_command_menu;
            state.message = DashboardMessage::info(if state.show_command_menu {
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
            export_dashboard_snapshot(&snapshot, args.stale_after, &path)?;
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
        .env("PORTMAP_OLD_PORT", from_port.to_string())
        .env("PORTMAP_NEW_PORT", to_port.to_string())
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
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else {
        bail!("open is not supported on this platform");
    };

    let status = Command::new(program)
        .arg(url)
        .status()
        .with_context(|| format!("failed to execute {}", program))?;
    if !status.success() {
        bail!("{} returned non-zero status for {}", program, url);
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

fn render_dashboard(stdout: &mut io::Stdout, state: &DashboardState, args: &MapArgs) -> Result<()> {
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
    state: &DashboardState,
    args: &MapArgs,
    width: usize,
    height: usize,
) -> Vec<String> {
    let summary = build_dashboard_summary(&state.snapshot);
    let free_port_list = free_ports(
        DEFAULT_AVAILABLE_FROM,
        DEFAULT_AVAILABLE_TO,
        &state.snapshot.active,
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
        .snapshot
        .state
        .ports
        .values()
        .filter(|entry| entry.last_status == PersistedStatus::Released)
        .count();

    let (list_height, detail_height) = dashboard_layout(height, !state.rows.is_empty());

    let mut lines = Vec::with_capacity(height);
    lines.push(format!(
        "portmap dashboard  ports:{}  processes:{}  stale:{}  projects:{}",
        summary.ports, summary.processes, summary.stale, summary.projects
    ));
    lines.push(format!(
        "free:{}  released:{}  refresh:{}  stale-after:{}  filter:{}  sort:{}  visible:{}/{}",
        free_ports,
        released_count,
        format_duration_short(args.refresh_every),
        format_duration_short(args.stale_after),
        state.filter_label(),
        state.sort_label(),
        state.rows.len(),
        state.all_rows.len()
    ));
    lines.push(separator_line(width));
    lines.push("Sessions".to_string());
    lines.push(format_session_header(width));

    let start = visible_row_start(state.selected, state.rows.len(), list_height);
    for offset in 0..list_height {
        let index = start + offset;
        if let Some(row) = state.rows.get(index) {
            lines.push(format_session_row(
                row,
                width,
                index == state.selected,
                state.health_for(row),
            ));
        } else if state.rows.is_empty() && offset == 0 {
            lines.push(
                "  No rows match current filter. Use `clear`, `filter ...`, or `refresh`."
                    .to_string(),
            );
        } else {
            lines.push(String::new());
        }
    }

    lines.push(separator_line(width));
    lines.push(if state.show_command_menu {
        "Command Menu".to_string()
    } else {
        "Details".to_string()
    });

    let detail_lines = if state.show_command_menu {
        build_command_menu_lines()
    } else {
        build_detail_lines(state)
    };
    for index in 0..detail_height {
        lines.push(detail_lines.get(index).cloned().unwrap_or_default());
    }

    lines.push(separator_line(width));
    lines.push("Hotkeys: ?/F1 toggle menu, Esc closes menu/clears input, Ctrl+C quit".to_string());
    lines.push(format!("Status: {}", state.message.render()));
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

fn build_detail_lines(state: &DashboardState) -> Vec<String> {
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

fn build_command_menu_lines() -> Vec<String> {
    vec![
        "Session actions".to_string(),
        "  restart [pid|port] [--port N]    restart selected/target session".to_string(),
        "  quick stale                      kill stale sessions".to_string(),
        "  quick old                        kill sessions older than 2h".to_string(),
        "  quick restart-old                restart sessions older than 2h".to_string(),
        "  kill [port|pid]                  terminate selected/target".to_string(),
        "  move <new-port>                  move selected session".to_string(),
        "  move <from-port> <new-port>      move specific port owner".to_string(),
        "Discovery and view".to_string(),
        "  filter <expr>                    stale|active|health:*|port:*|project:*|cmd:*"
            .to_string(),
        "  sort <key> [asc|desc]            port|project|age|state|health".to_string(),
        "  sort new-old | sort old-new      age shorthands".to_string(),
        "  clear                            reset filter + sort".to_string(),
        "  open [port|pid]                  open selected/target URL in browser".to_string(),
        "  select <port|pid>                jump to row".to_string(),
        "System".to_string(),
        "  refresh                          force refresh now".to_string(),
        "  download [file]                  export JSON snapshot".to_string(),
        "  help                             show short help in status".to_string(),
        "  quit                             exit dashboard".to_string(),
        "Hotkeys".to_string(),
        "  ? or F1                          toggle this menu".to_string(),
        "  Esc                              close menu or clear input".to_string(),
        "  Ctrl+C                           quit".to_string(),
    ]
}

fn format_session_header(width: usize) -> String {
    truncate_end(
        "  Port  State  Health  Age   PID   PPID TTY      Project           URL",
        width,
    )
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
    let fixed_lines = 11usize;
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

fn separator_line(width: usize) -> String {
    "─".repeat(width.max(1))
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

    let export = DashboardExport {
        exported_at_epoch: now_epoch(),
        cwd: std::env::current_dir().context("failed to determine current directory")?,
        stale_after_seconds: stale_after.as_secs(),
        summary,
        free_ports,
        active,
        recent_released,
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
    use tempfile::tempdir;

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
}
