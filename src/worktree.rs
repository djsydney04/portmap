use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone)]
pub struct RepoSnapshot {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub base_ref: Option<String>,
    pub worktrees: Vec<WorktreeRecord>,
}

#[derive(Debug, Clone)]
pub struct WorktreeRecord {
    pub path: PathBuf,
    pub is_main: bool,
    pub bare: bool,
    pub head_oid: String,
    pub branch: Option<String>,
    pub detached: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable: bool,
    pub prunable_reason: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub origin_ref: Option<String>,
    pub origin_ahead: u32,
    pub origin_behind: u32,
    pub base_ref: Option<String>,
    pub base_ahead: u32,
    pub base_behind: u32,
    pub merged_into_base: Option<bool>,
    pub staged: Vec<String>,
    pub unstaged: Vec<String>,
    pub untracked: Vec<String>,
    pub conflicted: Vec<String>,
    pub operations: Vec<WorktreeOperation>,
    pub last_commit: Option<CommitRecord>,
    pub last_checkout_epoch: Option<i64>,
    pub git_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct CommitRecord {
    pub short_oid: String,
    pub committed_at_epoch: i64,
    pub subject: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorktreeOperation {
    Rebase,
    Merge,
    CherryPick,
}

impl WorktreeOperation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rebase => "rebase",
            Self::Merge => "merge",
            Self::CherryPick => "cherry-pick",
        }
    }
}

#[derive(Debug, Default)]
struct RawWorktree {
    path: Option<PathBuf>,
    head_oid: Option<String>,
    branch: Option<String>,
    detached: bool,
    locked: bool,
    lock_reason: Option<String>,
    prunable: bool,
    prunable_reason: Option<String>,
    bare: bool,
}

pub fn discover_current_repo() -> Result<Option<RepoSnapshot>> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    discover_repo_from(&cwd)
}

pub fn discover_repo_from(cwd: &Path) -> Result<Option<RepoSnapshot>> {
    let Some(root_raw) = git_stdout(&cwd, &["rev-parse", "--show-toplevel"]).ok() else {
        return Ok(None);
    };
    let root = normalize_existing_path(PathBuf::from(root_raw));
    let common_dir_raw = git_stdout(&root, &["rev-parse", "--git-common-dir"])
        .context("failed to query git common dir")?;
    let common_dir = normalize_existing_path(absolutize_git_path(&root, &common_dir_raw));
    let base_ref = discover_base_ref(&root);
    let listing = git_output(&root, &["worktree", "list", "--porcelain", "-z"])
        .context("failed to list git worktrees")?;
    let raw_worktrees = parse_worktree_list(&listing.stdout)?;
    let mut worktrees = Vec::with_capacity(raw_worktrees.len());

    for raw in raw_worktrees {
        let path = raw
            .path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("worktree list entry is missing a path"))?;
        let path = normalize_existing_path(path);
        let mut record = WorktreeRecord {
            is_main: path == root,
            path,
            bare: raw.bare,
            head_oid: raw.head_oid.unwrap_or_default(),
            branch: raw.branch,
            detached: raw.detached,
            locked: raw.locked,
            lock_reason: raw.lock_reason,
            prunable: raw.prunable,
            prunable_reason: raw.prunable_reason,
            upstream: None,
            ahead: 0,
            behind: 0,
            origin_ref: None,
            origin_ahead: 0,
            origin_behind: 0,
            base_ref: None,
            base_ahead: 0,
            base_behind: 0,
            merged_into_base: None,
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: Vec::new(),
            conflicted: Vec::new(),
            operations: Vec::new(),
            last_commit: None,
            last_checkout_epoch: None,
            git_dir: None,
        };

        if record.prunable || !record.path.exists() || record.bare {
            worktrees.push(record);
            continue;
        }

        record.git_dir = discover_git_dir(&record.path).ok();
        record.upstream = discover_upstream(&record.path);
        if let Some(upstream) = record.upstream.as_deref() {
            let (ahead, behind) = discover_ahead_behind(&record.path, upstream).unwrap_or((0, 0));
            record.ahead = ahead;
            record.behind = behind;
        }
        if let Some(branch) = record.branch.as_deref() {
            if let Some(origin_ref) = discover_origin_branch_ref(&record.path, branch) {
                let (ahead, behind) =
                    discover_ahead_behind(&record.path, &origin_ref).unwrap_or((0, 0));
                record.origin_ref = Some(origin_ref);
                record.origin_ahead = ahead;
                record.origin_behind = behind;
            }
        }
        if let Some(base_ref) = base_ref.as_deref() {
            record.base_ref = Some(base_ref.to_string());
            if let Ok((ahead, behind)) = discover_ahead_behind(&record.path, base_ref) {
                record.base_ahead = ahead;
                record.base_behind = behind;
            }
            record.merged_into_base = discover_is_ancestor(&record.path, "HEAD", base_ref).ok();
        }
        record.staged =
            git_path_lines(&record.path, &["diff", "--name-only", "--cached"]).unwrap_or_default();
        record.unstaged =
            git_path_lines(&record.path, &["diff", "--name-only"]).unwrap_or_default();
        record.untracked = git_path_lines(
            &record.path,
            &["ls-files", "--others", "--exclude-standard"],
        )
        .unwrap_or_default();
        record.conflicted =
            git_path_lines(&record.path, &["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default();
        normalize_file_lists(&mut record);
        record.operations = detect_operations(record.git_dir.as_deref());
        record.last_commit = discover_last_commit(&record.path).ok();
        record.last_checkout_epoch = discover_last_checkout_epoch(&record.path).unwrap_or(None);
        worktrees.push(record);
    }

    Ok(Some(RepoSnapshot {
        root,
        common_dir,
        base_ref,
        worktrees,
    }))
}

pub fn discover_git_dir(path: &Path) -> Result<PathBuf> {
    let raw = git_stdout(path, &["rev-parse", "--git-dir"]).context("failed to query git dir")?;
    Ok(absolutize_git_path(path, &raw))
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to execute git {:?}", args))
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = git_output(cwd, args)?;
    if !output.status.success() {
        bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_path_lines(cwd: &Path, args: &[&str]) -> Result<Vec<String>> {
    let output = git_stdout(cwd, args)?;
    let mut lines = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    lines.sort();
    lines.dedup();
    Ok(lines)
}

fn parse_worktree_list(bytes: &[u8]) -> Result<Vec<RawWorktree>> {
    let mut records = Vec::new();
    let mut current = RawWorktree::default();

    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            if current.path.is_some() {
                records.push(std::mem::take(&mut current));
            }
            continue;
        }

        let value = String::from_utf8(field.to_vec())
            .context("git worktree list returned non-utf8 data")?;
        if let Some(path) = value.strip_prefix("worktree ") {
            if current.path.is_some() {
                records.push(std::mem::take(&mut current));
            }
            current.path = Some(PathBuf::from(path));
        } else if let Some(head_oid) = value.strip_prefix("HEAD ") {
            current.head_oid = Some(head_oid.to_string());
        } else if let Some(branch) = value.strip_prefix("branch ") {
            current.branch = Some(short_branch_name(branch));
        } else if value == "detached" {
            current.detached = true;
        } else if value == "bare" {
            current.bare = true;
        } else if let Some(reason) = value.strip_prefix("locked ") {
            current.locked = true;
            current.lock_reason = Some(reason.to_string());
        } else if value == "locked" {
            current.locked = true;
        } else if let Some(reason) = value.strip_prefix("prunable ") {
            current.prunable = true;
            current.prunable_reason = Some(reason.to_string());
        } else if value == "prunable" {
            current.prunable = true;
        }
    }

    if current.path.is_some() {
        records.push(current);
    }

    Ok(records)
}

fn short_branch_name(value: &str) -> String {
    value
        .strip_prefix("refs/heads/")
        .or_else(|| value.strip_prefix("refs/remotes/"))
        .unwrap_or(value)
        .to_string()
}

fn absolutize_git_path(base: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn normalize_existing_path(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn discover_upstream(path: &Path) -> Option<String> {
    git_stdout(
        path,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .ok()
}

fn discover_ahead_behind(path: &Path, upstream: &str) -> Result<(u32, u32)> {
    let raw = git_stdout(
        path,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...{upstream}"),
        ],
    )?;
    let mut parts = raw.split_whitespace();
    let ahead = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing ahead count"))?
        .parse::<u32>()
        .context("invalid ahead count")?;
    let behind = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing behind count"))?
        .parse::<u32>()
        .context("invalid behind count")?;
    Ok((ahead, behind))
}

fn discover_base_ref(path: &Path) -> Option<String> {
    for candidate in ["main", "origin/main", "master", "origin/master"] {
        if git_ref_exists(path, candidate) {
            return Some(candidate.to_string());
        }
    }

    let origin_head = git_stdout(path, &["symbolic-ref", "refs/remotes/origin/HEAD"]).ok()?;
    let suffix = origin_head.strip_prefix("refs/remotes/")?;
    if git_ref_exists(path, suffix) {
        Some(suffix.to_string())
    } else {
        None
    }
}

fn discover_origin_branch_ref(path: &Path, branch: &str) -> Option<String> {
    let origin_ref = format!("origin/{branch}");
    if git_ref_exists(path, &origin_ref) {
        Some(origin_ref)
    } else {
        None
    }
}

fn git_ref_exists(path: &Path, reference: &str) -> bool {
    git_output(path, &["rev-parse", "--verify", "--quiet", reference])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn discover_is_ancestor(path: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = git_output(path, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base --is-ancestor {} {} failed: {}",
            ancestor,
            descendant,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn detect_operations(git_dir: Option<&Path>) -> Vec<WorktreeOperation> {
    let Some(git_dir) = git_dir else {
        return Vec::new();
    };

    let mut ops = BTreeSet::new();
    if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
        ops.insert(WorktreeOperation::Rebase);
    }
    if git_dir.join("MERGE_HEAD").exists() {
        ops.insert(WorktreeOperation::Merge);
    }
    if git_dir.join("CHERRY_PICK_HEAD").exists() {
        ops.insert(WorktreeOperation::CherryPick);
    }
    ops.into_iter().collect()
}

fn discover_last_commit(path: &Path) -> Result<CommitRecord> {
    let raw = git_stdout(path, &["log", "-1", "--format=%H%x1f%h%x1f%ct%x1f%s"])?;
    let mut parts = raw.split('\x1f');
    let _full_oid = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing commit oid"))?;
    let short_oid = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing short oid"))?
        .to_string();
    let committed_at_epoch = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing commit time"))?
        .parse::<i64>()
        .context("invalid commit time")?;
    let subject = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing commit subject"))?
        .to_string();

    Ok(CommitRecord {
        short_oid,
        committed_at_epoch,
        subject,
    })
}

fn discover_last_checkout_epoch(path: &Path) -> Result<Option<i64>> {
    let raw = git_stdout(
        path,
        &["reflog", "--date=unix", "--format=%ct%x1f%gs", "-n", "64"],
    )?;
    for line in raw.lines() {
        let Some((epoch, message)) = line.split_once('\x1f') else {
            continue;
        };
        if !(message.starts_with("checkout:") || message.starts_with("switch:")) {
            continue;
        }
        let parsed = epoch.parse::<i64>().context("invalid reflog epoch")?;
        return Ok(Some(parsed));
    }
    Ok(None)
}

fn normalize_file_lists(record: &mut WorktreeRecord) {
    let conflicted: BTreeSet<_> = record.conflicted.iter().cloned().collect();
    record.staged.retain(|path| !conflicted.contains(path));
    record.unstaged.retain(|path| !conflicted.contains(path));
}

pub fn active_operation(record: &WorktreeRecord) -> Option<WorktreeOperation> {
    match record.operations.as_slice() {
        [operation] => Some(*operation),
        _ => None,
    }
}

pub fn run_git(cwd: &Path, args: &[String]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("failed to execute git {:?}", args))?;
    if !status.success() {
        bail!("git {:?} returned {}", args, status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worktree_list_records() {
        let input = b"worktree /tmp/repo\0HEAD abc\0branch refs/heads/main\0\0worktree /tmp/repo-feature\0HEAD def\0detached\0locked because\0\0";
        let records = parse_worktree_list(input).expect("records");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].path.as_deref(), Some(Path::new("/tmp/repo")));
        assert_eq!(records[0].branch.as_deref(), Some("main"));
        assert!(records[1].detached);
        assert!(records[1].locked);
        assert_eq!(records[1].lock_reason.as_deref(), Some("because"));
    }
}
