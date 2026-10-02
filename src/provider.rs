//! Subprocess/HTTP/builtin fetches, all bounded by a deadline and a byte cap.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::time::Instant;

use crate::config::{BadgeConfig, Builtin, GitField, Source};
use crate::extract;

#[derive(Debug, Clone)]
pub enum ProviderError {
    Timeout,
    Stopped,
    TooLarge,
    Spawn(String),
    Io(String),
    NonZeroExit(i32),
    Http(String),
    HttpStatus(u16),
    Extract(String),
    NoBattery,
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(f, "timed out"),
            Self::Stopped => write!(f, "daemon is stopping"),
            Self::TooLarge => write!(f, "output exceeded max_output"),
            Self::Spawn(e) => write!(f, "spawn failed: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::NonZeroExit(code) => write!(f, "exited with status {code}"),
            Self::Http(e) => write!(f, "http error: {e}"),
            Self::HttpStatus(code) => write!(f, "http status {code}"),
            Self::Extract(e) => write!(f, "extract error: {e}"),
            Self::NoBattery => write!(f, "no battery found"),
        }
    }
}

/// A `git status` result shared only while its subprocess is running. The sender stores the
/// completed result long enough for current waiters to receive it, but the map entry is
/// removed before publication so a later badge refresh always starts a fresh status check.
struct PorcelainFlight {
    result: watch::Sender<Option<Result<Arc<[u8]>, ProviderError>>>,
}

/// State shared by every fetch: one pooled HTTP client, the in-flight process-group
/// registry (killed on stop/signal/idle-exit), a global concurrency cap, and the in-flight-only
/// per-repo `git_counts` singleflight map (see `porcelain_fetch`).
#[derive(Clone)]
pub struct Shared {
    pub http: reqwest::Client,
    pub inflight: Arc<Mutex<HashSet<i32>>>,
    pub semaphore: Arc<Semaphore>,
    pub blocking: crate::blocking::Pool,
    stopping: Arc<AtomicBool>,
    porcelain: Arc<Mutex<HashMap<PathBuf, Arc<PorcelainFlight>>>>,
}

impl Shared {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("static client config is valid");
        Self {
            http,
            inflight: Arc::new(Mutex::new(HashSet::new())),
            semaphore: Arc::new(Semaphore::new(4)),
            blocking: crate::blocking::Pool::new(),
            stopping: Arc::new(AtomicBool::new(false)),
            porcelain: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Close admission before killing groups. A spawn already blocked in the OS checks
    /// this gate again when it returns, and kills its group instead of registering it.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.semaphore.close();
        let pgids = self.inflight.lock().unwrap();
        for &pgid in pgids.iter() {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
}

const GIT_ENV: &[(&str, &str)] = &[("GIT_OPTIONAL_LOCKS", "0")];
const NO_ENV: &[(&str, &str)] = &[];

/// Extra env vars and the shared in-flight registry, bundled to keep `run_with_timeout`'s
/// argument count sane.
struct RunOpts<'a> {
    envs: &'a [(&'a str, &'a str)],
    shared: &'a Shared,
}

/// Own the group together with the child, including while a blocking spawn's result
/// is waiting for delivery. Cancellation or a late result must kill descendants too.
struct SpawnedChild {
    child: Child,
    pgid: i32,
    inflight: Arc<Mutex<HashSet<i32>>>,
    armed: bool,
}

impl SpawnedChild {
    fn kill_group(&self) {
        if self.armed {
            unsafe { libc::killpg(self.pgid, libc::SIGKILL) };
        }
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let result = self.child.wait().await;
        if result.is_ok() {
            self.armed = false;
            self.inflight.lock().unwrap().remove(&self.pgid);
        }
        result
    }
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        self.kill_group();
        self.inflight.lock().unwrap().remove(&self.pgid);
        // Child's kill_on_drop reaps the leader through tokio's orphan reaper.
    }
}

/// Runs `prog args` (stdin from `/dev/null`), killing the whole process group on timeout
/// or oversized stdout. The capped stdout read and `wait()` both happen inside `deadline`; on the success path the leader is reaped and never killed (its pgid
/// may be reused once dead).
async fn run_with_timeout(
    prog: &str,
    args: &[String],
    cwd: Option<&Path>,
    deadline: Instant,
    cap: usize,
    opts: RunOpts<'_>,
) -> Result<Vec<u8>, ProviderError> {
    if opts.shared.stopping.load(Ordering::SeqCst) {
        return Err(ProviderError::Stopped);
    }
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in opts.envs {
        cmd.env(k, v);
    }

    let shared = opts.shared.clone();
    let mut child = opts
        .shared
        .blocking
        .run(deadline, move || {
            if shared.stopping.load(Ordering::SeqCst) {
                return Err(ProviderError::Stopped);
            }
            // Command::spawn waits for exec/chdir and can itself hang on a mount.
            let child = cmd
                .spawn()
                .map_err(|e| ProviderError::Spawn(e.to_string()))?;
            let pgid = child
                .id()
                .ok_or_else(|| ProviderError::Spawn("child has no pid".into()))?
                .cast_signed();
            let spawned = SpawnedChild {
                child,
                pgid,
                inflight: Arc::clone(&shared.inflight),
                armed: true,
            };
            {
                let mut registry = shared.inflight.lock().unwrap();
                if shared.stopping.load(Ordering::SeqCst) {
                    // Drop outside the registry lock: the guard also removes its pid.
                    drop(registry);
                    return Err(ProviderError::Stopped);
                }
                registry.insert(pgid);
            }
            Ok(spawned)
        })
        .await
        .ok_or(ProviderError::Timeout)??;

    let mut stdout = child.child.stdout.take().expect("stdout is piped");

    let work = async {
        match read_capped(&mut stdout, cap).await {
            Ok(out) => match child.wait().await {
                Ok(status) if status.success() => Ok(out),
                Ok(status) => Err(ProviderError::NonZeroExit(status.code().unwrap_or(-1))),
                Err(e) => Err(ProviderError::Io(e.to_string())),
            },
            Err(e) => {
                // Read stopped early (e.g. TooLarge): the child may be blocked writing to
                // a full pipe and would never exit on its own, so kill before reaping.
                child.kill_group();
                let _ = child.wait().await;
                Err(e)
            }
        }
    };

    match tokio::time::timeout_at(deadline, work).await {
        Ok(r) => r,
        Err(_elapsed) => {
            // The leader is not yet reaped here (zombie at worst), so its pgid cannot
            // have been recycled.
            child.kill_group();
            let _ = child.wait().await;
            Err(ProviderError::Timeout)
        }
    }
}

async fn read_capped(stdout: &mut ChildStdout, cap: usize) -> Result<Vec<u8>, ProviderError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = stdout
            .read(&mut chunk)
            .await
            .map_err(|e| ProviderError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > cap {
            return Err(ProviderError::TooLarge);
        }
    }
    Ok(buf)
}

async fn http_fetch(
    client: &reqwest::Client,
    url: &str,
    headers: &BTreeMap<String, String>,
    deadline: Instant,
    cap: usize,
) -> Result<Vec<u8>, ProviderError> {
    let mut builder = client.get(url);
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    let fut = async {
        let mut resp = builder
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ProviderError::HttpStatus(resp.status().as_u16()));
        }
        let mut buf = Vec::new();
        while let Some(part) = resp
            .chunk()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?
        {
            buf.extend_from_slice(&part);
            if buf.len() > cap {
                return Err(ProviderError::TooLarge);
            }
        }
        Ok(buf)
    };
    tokio::time::timeout_at(deadline, fut)
        .await
        .unwrap_or(Err(ProviderError::Timeout))
}

// Adapted from beachcomber `src/provider/git.rs` `find_repo_root` (MIT, Copyright (c) 2026
// Joshua Hogendorn); see THIRD_PARTY_NOTICES.md.
/// Walks up from `start` looking for a `.git` entry (dir, or file for worktrees). In-process
/// `stat`s only, never a subprocess. `None` means `start` is not inside a repo.
pub fn git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

// Adapted from beachcomber `src/provider/git.rs` `resolve_git_dir` (MIT, Copyright (c) 2026
// Joshua Hogendorn); see THIRD_PARTY_NOTICES.md.
/// Resolves the actual git directory for `repo_root`: a `.git` directory (including a symlink
/// to one), or (for worktrees) the target of a `.git` *file*'s `gitdir: ...` line. In-process
/// only.
pub fn git_dir(repo_root: &Path) -> Option<PathBuf> {
    let dot_git = repo_root.join(".git");
    // `metadata` follows a `.git` symlink so symlinked worktrees resolve to their target
    // directory. A broken symlink remains an error and therefore isn't mistaken for a gitdir.
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let target = contents.trim().strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(target);
    Some(if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    })
}

/// mtimes of `.git/HEAD` and `.git/index` (gitdir-resolved): a cheap, subprocess-free proxy
/// for "has the repo's branch or stage changed". Used by the outer cache to invalidate the
/// two git builtins still backed by a subprocess (`git_status`/`git_counts`, see
/// `cache::Entry::fingerprint`) the instant either file changes, instead of waiting up to
/// `interval`.
///
/// ponytail: doesn't see unstaged worktree edits (they touch neither `HEAD` nor `index`), so
/// those still only surface on `interval` — documented in README; the upgrade path if that
/// ever matters enough is inotify on the worktree, at the cost of a watcher thread this
/// design otherwise avoids entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GitFingerprint {
    head: Option<SystemTime>,
    index: Option<SystemTime>,
}

pub fn git_fingerprint(dir: &Path) -> GitFingerprint {
    let mtime = |p: &Path| std::fs::metadata(p).ok().and_then(|m| m.modified().ok());
    GitFingerprint {
        head: mtime(&dir.join("HEAD")),
        index: mtime(&dir.join("index")),
    }
}

// Adapted from beachcomber `src/provider/git.rs` `resolve_git_dir` (MIT, Copyright (c) 2026
// Joshua Hogendorn); see THIRD_PARTY_NOTICES.md.
/// Resolves the shared git dir from a (possibly per-worktree) git dir: a worktree's git dir
/// contains a `commondir` file pointing back at the main `.git` dir, which holds state shared
/// across worktrees (e.g. `refs/stash`); state that's per-worktree (`HEAD`, `MERGE_HEAD`,
/// `rebase-merge`, …) stays in `git_dir` itself.
fn common_git_dir(git_dir: &Path) -> PathBuf {
    std::fs::read_to_string(git_dir.join("commondir")).map_or_else(
        |_| git_dir.to_path_buf(),
        |contents| {
            let target = PathBuf::from(contents.trim());
            if target.is_absolute() {
                target
            } else {
                git_dir.join(target)
            }
        },
    )
}

/// Resolves `HEAD`'s full commit SHA in-process: a loose ref file (following bounded symbolic
/// ref chains), falling back to `packed-refs`. `None` on an unborn branch (no commits yet) or
/// anything unparseable.
/// ponytail: takes the literal first 7 chars as the "short" hash rather than git's
/// shortest-unique-prefix algorithm — good enough for a display badge, not `git`-exact.
fn read_head_commit(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(ref_name) = head.strip_prefix("ref:") else {
        let is_sha = head.len() >= 4 && head.bytes().all(|b| b.is_ascii_hexdigit());
        return is_sha.then(|| head.to_string());
    };
    let ref_name = ref_name.trim();
    let common = common_git_dir(git_dir);
    let mut ref_name = ref_name.to_string();
    let mut seen = HashSet::new();
    // Git symbolic refs are normally one hop, but malformed/cyclic chains must not make this
    // in-process reader loop forever.
    for _ in 0..5 {
        if !seen.insert(ref_name.clone()) {
            return None;
        }
        if let Ok(contents) = std::fs::read_to_string(common.join(&ref_name)) {
            let value = contents.trim();
            if let Some(target) = value.strip_prefix("ref:") {
                ref_name = target.trim().to_string();
                continue;
            }
            return Some(value.to_string());
        }
        // A packed ref cannot itself be symbolic, so a match ends the traversal.
        let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
        return packed.lines().find_map(|line| {
            let (sha, name) = line.split_once(' ')?;
            (name == ref_name).then(|| sha.to_string())
        });
    }
    None
}

// Adapted from beachcomber `src/provider/git.rs` `detect_repo_state` (MIT, Copyright (c) 2026
// Joshua Hogendorn); see THIRD_PARTY_NOTICES.md.
/// `REBASING`/`MERGING`/`CHERRY-PICKING`/`BISECTING`, detected from the presence of the usual
/// `.git` state markers; empty when no operation is in progress.
fn git_state(git_dir: &Path) -> &'static str {
    if git_dir.join("rebase-merge").is_dir() || git_dir.join("rebase-apply").is_dir() {
        "REBASING"
    } else if git_dir.join("MERGE_HEAD").exists() {
        "MERGING"
    } else if git_dir.join("CHERRY_PICK_HEAD").exists() {
        "CHERRY-PICKING"
    } else if git_dir.join("BISECT_LOG").exists() {
        "BISECTING"
    } else {
        ""
    }
}

// Adapted from beachcomber `src/provider/git.rs` `count_stashes` (MIT, Copyright (c) 2026
// Joshua Hogendorn); see THIRD_PARTY_NOTICES.md.
/// Number of stash entries, counted from the stash reflog (one line per `git stash push`).
/// `common_git_dir` lives on the shared dir, so this is correct from any worktree.
fn git_stash_count(common_git_dir: &Path) -> u32 {
    std::fs::read_to_string(common_git_dir.join("logs/refs/stash")).map_or(0, |s| {
        u32::try_from(s.lines().filter(|l| !l.is_empty()).count()).unwrap_or(u32::MAX)
    })
}

// Adapted from beachcomber `src/provider/git.rs` `GitHead::parse_head` (MIT, Copyright (c) 2026
// Joshua Hogendorn); see THIRD_PARTY_NOTICES.md.
/// Branch name parsed from `HEAD`'s `ref: refs/heads/<name>` line; empty for anything else
/// (detached HEAD, or a ref outside `refs/heads/`) — matches `git branch --show-current`.
fn git_branch_from_head(git_dir: &Path) -> String {
    let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) else {
        return String::new();
    };
    head.trim()
        .strip_prefix("ref:")
        .map(str::trim)
        .and_then(|r| r.strip_prefix("refs/heads/"))
        .map_or_else(String::new, str::to_string)
}

/// In-process fast path for the four git builtins that are pure file reads
/// (`git_branch`/`git_commit`/`git_state`/`git_stash`): cheap enough (microseconds) to
/// recompute on every request instead of going through the cache/fetch machinery, which
/// gives them the same observable freshness as a real fs watcher without running one (see
/// `daemon::handle_get`, design.md §6). `Some(_)` (possibly empty bytes — outside a repo,
/// detached HEAD, etc.) for these four builtins; `None` for everything else, which still
/// goes through the normal cache + (for `git_status`/`git_counts`) subprocess path.
pub fn run_in_process_git(name: Builtin, git_dir: Option<&Path>) -> Option<Vec<u8>> {
    if !matches!(
        name,
        Builtin::GitBranch | Builtin::GitCommit | Builtin::GitState | Builtin::GitStash
    ) {
        return None;
    }
    let Some(dir) = git_dir else {
        return Some(Vec::new());
    };
    Some(match name {
        Builtin::GitBranch => git_branch_from_head(dir).into_bytes(),
        Builtin::GitCommit => read_head_commit(dir)
            .map(|sha| sha.get(..7).unwrap_or(&sha).as_bytes().to_vec())
            .unwrap_or_default(),
        Builtin::GitState => git_state(dir).as_bytes().to_vec(),
        Builtin::GitStash => count_or_empty(git_stash_count(&common_git_dir(dir))),
        Builtin::GitStatus
        | Builtin::GitCounts
        | Builtin::Battery
        | Builtin::Hostname
        | Builtin::LoadAvg
        | Builtin::MemUsedPercent
        | Builtin::Uptime => unreachable!("checked above"),
    })
}

/// Status counts parsed from `git status --porcelain=v2 --branch`.
#[derive(Debug, Default, PartialEq, Eq)]
struct GitCounts {
    ahead: u32,
    behind: u32,
    staged: u32,
    modified: u32,
    untracked: u32,
    conflicted: u32,
    /// Every changed entry (`1`/`2`/`u`/`?` line), once each: what `git_status` reports.
    dirty: u32,
}

// Adapted from beachcomber `src/provider/git.rs` `parse_git_status` (MIT, Copyright (c) 2026
// Joshua Hogendorn); see THIRD_PARTY_NOTICES.md.
/// Parses `git status --porcelain=v2 --branch` output. Header lines (`#`) carry
/// `branch.ab +<ahead> -<behind>` (absent with no upstream); `1`/`2` file lines carry a 2-char
/// `XY` status (staged = `X != '.'`, modified = `Y != '.'`); `u` lines are always conflicted;
/// `?` lines are untracked.
fn parse_porcelain_v2(out: &[u8]) -> GitCounts {
    let text = String::from_utf8_lossy(out);
    let mut counts = GitCounts::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# branch.ab ") {
            let mut parts = rest.split_whitespace();
            counts.ahead = parts
                .next()
                .and_then(|s| s.strip_prefix('+'))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            counts.behind = parts
                .next()
                .and_then(|s| s.strip_prefix('-'))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("1 ").or_else(|| line.strip_prefix("2 ")) {
            let mut xy = rest.chars();
            if xy.next().is_some_and(|x| x != '.') {
                counts.staged += 1;
            }
            if xy.next().is_some_and(|y| y != '.') {
                counts.modified += 1;
            }
            counts.dirty += 1;
        } else if line.starts_with("u ") {
            counts.conflicted += 1;
            counts.dirty += 1;
        } else if line.starts_with("? ") {
            counts.untracked += 1;
            counts.dirty += 1;
        }
    }
    counts
}

/// `0` renders empty (design.md §5): the template adds any symbol, so e.g.
/// `format = "\u{2191}{value}"` shows nothing when a count is zero.
fn count_or_empty(n: u32) -> Vec<u8> {
    if n == 0 {
        Vec::new()
    } else {
        n.to_string().into_bytes()
    }
}

fn git_field_value(counts: &GitCounts, field: GitField) -> Vec<u8> {
    let n = match field {
        GitField::Ahead => counts.ahead,
        GitField::Behind => counts.behind,
        GitField::Staged => counts.staged,
        GitField::Modified => counts.modified,
        GitField::Untracked => counts.untracked,
        GitField::Conflicted => counts.conflicted,
    };
    count_or_empty(n)
}

#[cfg(any(not(target_os = "macos"), test))]
fn parse_kb_field(value: &str) -> Option<u64> {
    value.trim().strip_suffix("kB")?.trim().parse().ok()
}

/// Whole-percent memory usage from `/proc/meminfo`: `(MemTotal - MemAvailable) / MemTotal`.
/// `MemAvailable` (not `MemFree`) accounts for reclaimable cache, matching what `free`/`top`
/// report as "used".
#[cfg(any(not(target_os = "macos"), test))]
fn parse_meminfo_used_percent(contents: &str) -> Option<u64> {
    let mut total = None;
    let mut avail = None;
    for line in contents.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = parse_kb_field(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            avail = parse_kb_field(v);
        }
    }
    match (total, avail) {
        (Some(total), Some(avail)) if total > 0 => Some(total.saturating_sub(avail) * 100 / total),
        _ => None,
    }
}

/// The `host_statistics64(HOST_VM_INFO64)` page counts that make up "used" memory on macOS.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy)]
struct VmPages {
    internal: u64,
    purgeable: u64,
    wired: u64,
    compressed: u64,
}

/// Whole-percent memory usage on macOS, matching Activity Monitor's "Memory Used": app
/// memory (`internal - purgeable`) + wired + compressor-occupied pages, over `hw.memsize`.
/// Like Linux's `MemTotal - MemAvailable`, file cache and purgeable pages count as free.
#[cfg(any(target_os = "macos", test))]
fn vm_used_percent(pages: VmPages, page_size: u64, total_bytes: u64) -> Option<u64> {
    if total_bytes == 0 {
        return None;
    }
    let used_pages = pages
        .internal
        .saturating_sub(pages.purgeable)
        .saturating_add(pages.wired)
        .saturating_add(pages.compressed);
    let used = used_pages.saturating_mul(page_size).min(total_bytes);
    Some(used.saturating_mul(100) / total_bytes)
}

/// Absolute path, so a daemon inheriting an odd `$PATH` still finds it.
#[cfg(target_os = "macos")]
const PMSET: &str = "/usr/bin/pmset";
#[cfg(target_os = "macos")]
const PMSET_ARGS: [&str; 2] = ["-g", "batt"];
/// `pmset -g batt` prints well under 1 KiB; this only bounds a misbehaving binary.
#[cfg(target_os = "macos")]
const PMSET_CAP: usize = 64 * 1024;
/// Bound for `on_ac`'s synchronous probe, inside the daemon's 500 ms maintenance budget:
/// a hung `pmset` must not pin a blocking-pool slot.
#[cfg(target_os = "macos")]
const ON_AC_PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// `pmset -g batt`'s header (`Now drawing from 'AC Power'`) names the current source; any
/// other (`'Battery Power'`, `'UPS Power'`) is off AC. No header counts as AC, like Linux's
/// no-supply case.
#[cfg(any(target_os = "macos", test))]
fn pmset_on_ac(out: &str) -> bool {
    out.lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("Now drawing from '")?
                .split_once('\'')
        })
        .is_none_or(|(source, _)| source == "AC Power")
}

/// Charge percent from `pmset -g batt` source lines, e.g.
/// ` -InternalBattery-0 (id=4653155)\t95%; discharging; 4:12 remaining present: true`:
/// the source named exactly `device` (as printed, e.g. `InternalBattery-0` or a UPS name),
/// else the first `InternalBattery*` source.
#[cfg(any(target_os = "macos", test))]
fn pmset_battery_percent(out: &str, device: Option<&str>) -> Option<String> {
    out.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix('-')?;
        let name = rest
            .find(" (id=")
            .or_else(|| rest.find('\t'))
            .map_or(rest, |i| &rest[..i])
            .trim();
        let wanted = device.map_or_else(|| name.starts_with("InternalBattery"), |d| name == d);
        if !wanted {
            return None;
        }
        rest.split(|c: char| c.is_whitespace() || c == ';')
            .filter_map(|tok| tok.strip_suffix('%'))
            .find(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_owned)
    })
}

/// Runs `pmset -g batt` synchronously, killing it after `timeout`. Output (far below a
/// pipe buffer) is read only after exit, so the child never blocks on a full pipe.
#[cfg(target_os = "macos")]
fn pmset_batt_blocking(timeout: Duration) -> Option<String> {
    use std::io::Read as _;

    let mut child = std::process::Command::new(PMSET)
        .args(PMSET_ARGS)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if start.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

/// True unless `pmset -g batt` reports a non-AC source; a failed probe counts as AC.
#[cfg(target_os = "macos")]
pub fn on_ac() -> bool {
    pmset_batt_blocking(ON_AC_PROBE_TIMEOUT).is_none_or(|out| pmset_on_ac(&out))
}

/// True if any non-battery power supply (Mains, USB, Wireless, …) reports `online == 1`.
/// No such supply at all (desktop) is treated as AC.
#[cfg(not(target_os = "macos"))]
pub fn on_ac() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
        return true;
    };
    let mut saw_non_battery = false;
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
        if kind.trim() == "Battery" {
            continue;
        }
        saw_non_battery = true;
        let online = std::fs::read_to_string(path.join("online")).unwrap_or_default();
        if online.trim() == "1" {
            return true;
        }
    }
    !saw_non_battery
}

/// Removes only the generation it owns. An older task must not erase a newer flight for the
/// same root if the map entry was replaced while it was completing.
struct PorcelainCompletion {
    repo_root: PathBuf,
    flight: Arc<PorcelainFlight>,
    flights: Arc<Mutex<HashMap<PathBuf, Arc<PorcelainFlight>>>>,
    completed: bool,
}

impl PorcelainCompletion {
    fn finish(&mut self, result: Result<Vec<u8>, ProviderError>) {
        {
            let mut flights = self
                .flights
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if flights
                .get(&self.repo_root)
                .is_some_and(|current| Arc::ptr_eq(current, &self.flight))
            {
                flights.remove(&self.repo_root);
            }
        }
        self.flight
            .result
            .send_replace(Some(result.map(Arc::<[u8]>::from)));
        self.completed = true;
    }
}

impl Drop for PorcelainCompletion {
    fn drop(&mut self) {
        if !self.completed {
            self.finish(Err(ProviderError::Io(
                "git status singleflight task was cancelled".to_string(),
            )));
        }
    }
}

/// Shares a `git status` invocation only among callers that overlap in time. Freshness belongs
/// to the outer badge cache, so completed status output must never be retained here. The
/// semaphore permit is acquired only for a new flight: duplicate callers join the map before
/// admission and therefore cannot queue behind the process they are trying to share. The
/// creator's configured deadline and output cap bound the shared subprocess; each waiter also
/// enforces its own deadline and cap, but may fail if the creator's limits are tighter.
async fn porcelain_fetch_with<F, Fut>(
    shared: &Shared,
    repo_root: PathBuf,
    deadline: Instant,
    cap: usize,
    initialize: F,
) -> Result<Arc<[u8]>, ProviderError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<Vec<u8>, ProviderError>> + Send + 'static,
{
    if shared.stopping.load(Ordering::SeqCst) {
        return Err(ProviderError::Stopped);
    }
    let existing = { shared.porcelain.lock().unwrap().get(&repo_root).cloned() };
    if let Some(flight) = existing {
        return await_porcelain(&flight, deadline, cap).await;
    }

    // Only a new flight needs a process slot. Callers that arrive after insertion join it
    // without taking a permit, even when all slots are occupied.
    let permit = tokio::time::timeout_at(deadline, Arc::clone(&shared.semaphore).acquire_owned())
        .await
        .map_err(|_| ProviderError::Timeout)?
        .map_err(|_| ProviderError::Stopped)?;

    let (flight, newly_created) = {
        let mut flights = shared.porcelain.lock().unwrap();
        let flight = flights.get(&repo_root).cloned().map_or_else(
            || {
                let (sender, _) = watch::channel(None);
                let flight = Arc::new(PorcelainFlight { result: sender });
                flights.insert(repo_root.clone(), Arc::clone(&flight));
                (flight, true)
            },
            |flight| (flight, false),
        );
        drop(flights);
        flight
    };
    if !newly_created {
        drop(permit);
        return await_porcelain(&flight, deadline, cap).await;
    }

    let completion = PorcelainCompletion {
        repo_root: repo_root.clone(),
        flight: Arc::clone(&flight),
        flights: Arc::clone(&shared.porcelain),
        completed: false,
    };
    tokio::spawn(async move {
        let _permit: OwnedSemaphorePermit = permit;
        let mut completion = completion;
        let result = tokio::time::timeout_at(deadline, initialize())
            .await
            .unwrap_or(Err(ProviderError::Timeout))
            .and_then(|output| {
                if output.len() > cap {
                    Err(ProviderError::TooLarge)
                } else {
                    Ok(output)
                }
            });
        completion.finish(result);
    });
    await_porcelain(&flight, deadline, cap).await
}

async fn await_porcelain(
    flight: &PorcelainFlight,
    deadline: Instant,
    cap: usize,
) -> Result<Arc<[u8]>, ProviderError> {
    let mut result = flight.result.subscribe();
    let completed = tokio::time::timeout_at(deadline, async {
        loop {
            let completed = result.borrow_and_update().clone();
            if let Some(value) = completed {
                return value;
            }
            if result.changed().await.is_err() {
                return Err(ProviderError::Io(
                    "git status singleflight result was dropped".to_string(),
                ));
            }
        }
    })
    .await
    .map_err(|_| ProviderError::Timeout)?;
    completed.map(|output| {
        if output.len() > cap {
            Err(ProviderError::TooLarge)
        } else {
            Ok(output)
        }
    })?
}

async fn porcelain_fetch(
    shared: &Shared,
    repo_root: &Path,
    deadline: Instant,
    cap: usize,
) -> Result<Arc<[u8]>, ProviderError> {
    let cwd = repo_root.to_path_buf();
    let key = cwd.clone();
    let owned = shared.clone();
    porcelain_fetch_with(shared, key, deadline, cap, move || async move {
        let args = [
            "status".to_string(),
            "--porcelain=v2".to_string(),
            "--branch".to_string(),
        ];
        run_with_timeout(
            "git",
            &args,
            Some(&cwd),
            deadline,
            cap,
            RunOpts {
                envs: GIT_ENV,
                shared: &owned,
            },
        )
        .await
    })
    .await
}

/// `gethostname(3)` verbatim, i.e. what `hostname` prints: the kernel nodename on Linux;
/// on macOS often the Bonjour/DHCP name with its domain (e.g. `MacBook-Pro.local`). Use a
/// regex `extract` such as `^[^.]+` for the short form.
fn hostname() -> Result<String, ProviderError> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a live, writable buffer of the length passed.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return Err(ProviderError::Io(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(String::from_utf8_lossy(&buf[..len]).into_owned())
}

/// 1/5/15-minute load averages, two decimals each (same shape as `/proc/loadavg`).
fn load_avg() -> Result<String, ProviderError> {
    let mut avg = [0f64; 3];
    // SAFETY: `avg` holds exactly the 3 samples requested.
    if unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) } != 3 {
        return Err(ProviderError::Io("getloadavg failed".to_string()));
    }
    Ok(format!("{:.2} {:.2} {:.2}", avg[0], avg[1], avg[2]))
}

/// Whole seconds since boot, including time suspended — what `/proc/uptime` reports.
#[cfg(not(target_os = "macos"))]
fn uptime_secs() -> Result<u64, ProviderError> {
    // SAFETY: zeroed `timespec` is valid plain data, filled by the call below.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: `ts` is a live, writable `timespec`.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &raw mut ts) } != 0 {
        return Err(ProviderError::Io(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    u64::try_from(ts.tv_sec).map_err(|e| ProviderError::Io(e.to_string()))
}

/// Whole seconds since `kern.boottime`, including time asleep — what `uptime(1)` reports.
/// (`CLOCK_MONOTONIC` also counts sleep but has an unspecified origin; `CLOCK_UPTIME_RAW`
/// stops while asleep.)
#[cfg(target_os = "macos")]
fn uptime_secs() -> Result<u64, ProviderError> {
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let mut boot = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut len = size_of::<libc::timeval>();
    // SAFETY: `mib` names 2 levels; `boot`/`len` describe a live, writable `timeval`; no
    // new value is set.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut boot).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(ProviderError::Io(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let boot = u64::try_from(boot.tv_sec).map_err(|e| ProviderError::Io(e.to_string()))?;
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| ProviderError::Io(e.to_string()))?
        .as_secs();
    Ok(now.saturating_sub(boot))
}

#[cfg(not(target_os = "macos"))]
fn mem_used_percent() -> Result<u64, ProviderError> {
    let contents =
        std::fs::read_to_string("/proc/meminfo").map_err(|e| ProviderError::Io(e.to_string()))?;
    parse_meminfo_used_percent(&contents)
        .ok_or_else(|| ProviderError::Io("malformed /proc/meminfo".to_string()))
}

/// The host port, fetched once: every `mach_host_self` call adds a send-right reference.
#[cfg(target_os = "macos")]
fn mach_host() -> libc::mach_port_t {
    // Declared here: libc's binding is deprecated in favour of the `mach2` crate.
    unsafe extern "C" {
        fn mach_host_self() -> libc::mach_port_t;
    }
    // SAFETY: `mach_host_self` has no preconditions. (A closure, not the fn item:
    // `extern "C"` fns don't implement `FnOnce`.)
    static HOST: std::sync::LazyLock<libc::mach_port_t> =
        std::sync::LazyLock::new(|| unsafe { mach_host_self() });
    *HOST
}

#[cfg(target_os = "macos")]
fn mem_used_percent() -> Result<u64, ProviderError> {
    // Not bound by libc.
    unsafe extern "C" {
        fn host_page_size(
            host: libc::host_t,
            out_page_size: *mut libc::vm_size_t,
        ) -> libc::kern_return_t;
    }
    let mut total: u64 = 0;
    let mut len = size_of::<u64>();
    // SAFETY: the name is NUL-terminated; `total`/`len` describe a live, writable `u64`;
    // no new value is set.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut total).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(ProviderError::Io(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    // SAFETY: `vm_statistics64` is plain integer data; all-zero is a valid value.
    let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `stats` is a live, writable `vm_statistics64` spanning `count` `integer_t`s.
    let kr = unsafe {
        libc::host_statistics64(
            mach_host(),
            libc::HOST_VM_INFO64,
            (&raw mut stats).cast(),
            &raw mut count,
        )
    };
    if kr != libc::KERN_SUCCESS {
        return Err(ProviderError::Io(format!("host_statistics64 failed: {kr}")));
    }
    // `host_statistics64` counts kernel pages. Under Rosetta, `sysconf(_SC_PAGESIZE)`
    // reports 4 KiB while arm64 kernel pages are 16 KiB, which would read ~4x low;
    // `host_page_size` returns the kernel size (what `vm_stat` uses).
    let mut page_size: libc::vm_size_t = 0;
    // SAFETY: `page_size` is a live, writable `vm_size_t`.
    let kr = unsafe { host_page_size(mach_host(), &raw mut page_size) };
    if kr != libc::KERN_SUCCESS {
        return Err(ProviderError::Io(format!("host_page_size failed: {kr}")));
    }
    let page_size = u64::try_from(page_size).map_err(|e| ProviderError::Io(e.to_string()))?;
    let pages = VmPages {
        internal: u64::from(stats.internal_page_count),
        purgeable: u64::from(stats.purgeable_count),
        wired: u64::from(stats.wire_count),
        compressed: u64::from(stats.compressor_page_count),
    };
    vm_used_percent(pages, page_size, total)
        .ok_or_else(|| ProviderError::Io("hw.memsize is 0".to_string()))
}

/// `/sys/class/power_supply/<device>/capacity`, or the first supply whose `type` is
/// `Battery`.
#[cfg(not(target_os = "macos"))]
fn sysfs_battery(device: Option<String>) -> Result<Vec<u8>, ProviderError> {
    if let Some(dev) = device {
        let capacity = std::fs::read_to_string(format!("/sys/class/power_supply/{dev}/capacity"))
            .map_err(|e| ProviderError::Io(e.to_string()))?;
        return Ok(capacity.trim().as_bytes().to_vec());
    }
    let entries = std::fs::read_dir("/sys/class/power_supply")
        .map_err(|e| ProviderError::Io(e.to_string()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
        if kind.trim() != "Battery" {
            continue;
        }
        let capacity = std::fs::read_to_string(path.join("capacity"))
            .map_err(|e| ProviderError::Io(e.to_string()))?;
        return Ok(capacity.trim().as_bytes().to_vec());
    }
    Err(ProviderError::NoBattery)
}

#[cfg(not(target_os = "macos"))]
async fn battery(
    device: Option<&str>,
    deadline: Instant,
    shared: &Shared,
) -> Result<Vec<u8>, ProviderError> {
    let device = device.map(str::to_owned);
    shared
        .blocking
        .run(deadline, move || sysfs_battery(device))
        .await
        .ok_or(ProviderError::Timeout)?
}

/// No sysfs on macOS: parse `pmset -g batt`, run like any command so the badge deadline
/// kills its process group.
#[cfg(target_os = "macos")]
async fn battery(
    device: Option<&str>,
    deadline: Instant,
    shared: &Shared,
) -> Result<Vec<u8>, ProviderError> {
    let out = run_with_timeout(
        PMSET,
        &PMSET_ARGS.map(String::from),
        None,
        deadline,
        PMSET_CAP,
        RunOpts {
            envs: NO_ENV,
            shared,
        },
    )
    .await?;
    pmset_battery_percent(&String::from_utf8_lossy(&out), device)
        .map(String::into_bytes)
        .ok_or(ProviderError::NoBattery)
}

fn run_builtin_files(name: Builtin) -> Result<Vec<u8>, ProviderError> {
    match name {
        Builtin::Hostname => hostname().map(String::into_bytes),
        Builtin::LoadAvg => load_avg().map(String::into_bytes),
        Builtin::MemUsedPercent => mem_used_percent().map(|p| p.to_string().into_bytes()),
        Builtin::Uptime => uptime_secs().map(|s| s.to_string().into_bytes()),
        Builtin::Battery
        | Builtin::GitBranch
        | Builtin::GitCommit
        | Builtin::GitStatus
        | Builtin::GitState
        | Builtin::GitStash
        | Builtin::GitCounts => unreachable!("handled in run_builtin"),
    }
}

async fn run_builtin(
    name: Builtin,
    device: Option<&str>,
    field: Option<GitField>,
    cwd: Option<&Path>,
    deadline: Instant,
    cap: usize,
    shared: &Shared,
) -> Result<Vec<u8>, ProviderError> {
    match name {
        // In-process (no subprocess, no fetch-path cache entry): `daemon::handle_get`
        // intercepts these four before ever calling `fetch`/`fetch_raw`, so this arm is
        // unreachable in the real request path; the fallback stays correct and runs off the
        // reactor in case `fetch` is called directly.
        Builtin::GitBranch | Builtin::GitCommit | Builtin::GitState | Builtin::GitStash => {
            let cwd = cwd.map(Path::to_path_buf);
            shared
                .blocking
                .run(deadline, move || {
                    run_in_process_git(name, cwd.as_deref().and_then(git_dir).as_deref())
                        .unwrap_or_default()
                })
                .await
                .ok_or(ProviderError::Timeout)
        }
        // Both share one `git status --porcelain=v2 --branch` per repo (`porcelain_fetch`).
        // Raw value only (design.md §5): the template adds any symbol, and a zero count
        // renders empty.
        Builtin::GitStatus | Builtin::GitCounts => {
            let repo_root = cwd
                .ok_or_else(|| ProviderError::Io("git status requires a repo root".to_string()))?;
            let counts =
                parse_porcelain_v2(&porcelain_fetch(shared, repo_root, deadline, cap).await?);
            Ok(field.map_or_else(
                || count_or_empty(counts.dirty),
                |field| git_field_value(&counts, field),
            ))
        }
        Builtin::Battery => battery(device, deadline, shared).await,
        Builtin::Hostname | Builtin::LoadAvg | Builtin::MemUsedPercent | Builtin::Uptime => shared
            .blocking
            .run(deadline, move || run_builtin_files(name))
            .await
            .ok_or(ProviderError::Timeout)?,
    }
}

async fn fetch_raw(
    shared: &Shared,
    source: &Source,
    cwd: Option<&Path>,
    deadline: Instant,
    cap: usize,
) -> Result<Vec<u8>, ProviderError> {
    match source {
        Source::Builtin {
            name,
            device,
            field,
        } => run_builtin(*name, device.as_deref(), *field, cwd, deadline, cap, shared).await,
        Source::Command { command, args } => {
            // No args -> run through a shell so pipelines/`&&`/etc. work; the whole
            // pipeline is one process group, so a timeout's `killpg` covers every stage.
            let (prog, argv): (&str, Vec<String>) = args.as_ref().map_or_else(
                || ("sh", vec!["-c".to_string(), command.clone()]),
                |a| (command.as_str(), a.clone()),
            );
            run_with_timeout(
                prog,
                &argv,
                cwd,
                deadline,
                cap,
                RunOpts {
                    envs: NO_ENV,
                    shared,
                },
            )
            .await
        }
        Source::Http { url, headers } => {
            http_fetch(&shared.http, url, headers, deadline, cap).await
        }
    }
}

/// Fetches the badge's source, applies extraction, and renders the `{value}` template.
/// One global `Semaphore(4)` bounds fetch work; `git_status`/`git_counts` acquire its permit only for a
/// newly-created status subprocess so duplicate callers can join its in-flight work.
pub async fn fetch(
    shared: &Shared,
    badge: &BadgeConfig,
    cwd: Option<&Path>,
) -> Result<String, ProviderError> {
    if shared.stopping.load(Ordering::SeqCst) {
        return Err(ProviderError::Stopped);
    }
    let deadline = Instant::now() + badge.timeout;
    let _permit = if matches!(
        badge.source,
        Source::Builtin {
            name: Builtin::GitStatus | Builtin::GitCounts,
            ..
        }
    ) {
        None
    } else {
        Some(
            tokio::time::timeout_at(deadline, shared.semaphore.acquire())
                .await
                .map_err(|_| ProviderError::Timeout)?
                .map_err(|_| ProviderError::Stopped)?,
        )
    };

    let raw = fetch_raw(shared, &badge.source, cwd, deadline, badge.max_output).await?;
    let extraction = badge.extract.clone();
    let format = badge.format.clone();
    shared
        .blocking
        .run(deadline, move || {
            let extracted =
                extract::apply(extraction.as_ref(), &raw).map_err(ProviderError::Extract)?;
            Ok(extract::render(&format, &extracted))
        })
        .await
        .ok_or(ProviderError::Timeout)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_root_finds_dot_git_upward() {
        let dir = std::env::temp_dir().join(format!("sf-git-root-test-{}", std::process::id()));
        let nested = dir.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert_eq!(git_root(&nested), Some(dir.clone()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn git_root_none_outside_repo() {
        assert_eq!(git_root(Path::new("/")), None);
    }

    /// Unique temp dir per test, so parallel `cargo test` runs don't collide.
    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("sf-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn git_dir_resolves_plain_repo() {
        let root = temp_dir("gitdir-plain");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(git_dir(&root), Some(root.join(".git")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn git_dir_follows_absolute_dot_git_directory_symlink() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("gitdir-absolute-symlink");
        let target = root.join("actual-git-dir");
        let repo = root.join("repo");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        symlink(&target, repo.join(".git")).unwrap();
        assert_eq!(git_dir(&repo), Some(repo.join(".git")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn git_dir_follows_relative_dot_git_directory_symlink() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("gitdir-relative-symlink");
        let target = root.join("actual-git-dir");
        let repo = root.join("repo");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        symlink("../actual-git-dir", repo.join(".git")).unwrap();
        assert_eq!(git_dir(&repo), Some(repo.join(".git")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn git_dir_rejects_broken_dot_git_symlink() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("gitdir-broken-symlink");
        symlink("missing-git-dir", root.join(".git")).unwrap();
        assert_eq!(git_dir(&root), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn git_dir_follows_worktree_gitdir_file() {
        let root = temp_dir("gitdir-worktree");
        let real_dir = root.join("elsewhere/.git/worktrees/wt");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", real_dir.display()),
        )
        .unwrap();
        assert_eq!(git_dir(&root), Some(real_dir));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn common_git_dir_follows_commondir_file() {
        let dir = temp_dir("commondir");
        let main = dir.join("main-git");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::write(dir.join("commondir"), "../main-git\n").unwrap();
        assert_eq!(common_git_dir(&dir), dir.join("../main-git"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn common_git_dir_without_commondir_is_itself() {
        let dir = temp_dir("no-commondir");
        assert_eq!(common_git_dir(&dir), dir.clone());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_head_commit_resolves_loose_ref() {
        let dir = temp_dir("head-loose");
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(dir.join("refs/heads")).unwrap();
        std::fs::write(dir.join("refs/heads/main"), "abc123def456\n").unwrap();
        assert_eq!(read_head_commit(&dir), Some("abc123def456".to_string()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_head_commit_follows_symbolic_loose_ref_chain() {
        let dir = temp_dir("head-symbolic-chain");
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/alias\n").unwrap();
        std::fs::create_dir_all(dir.join("refs/heads")).unwrap();
        std::fs::write(dir.join("refs/heads/alias"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.join("refs/heads/main"), "abc123def456\n").unwrap();
        assert_eq!(read_head_commit(&dir), Some("abc123def456".to_string()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_head_commit_bounds_symbolic_ref_chains() {
        let dir = temp_dir("head-symbolic-loop");
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/one\n").unwrap();
        std::fs::create_dir_all(dir.join("refs/heads")).unwrap();
        std::fs::write(dir.join("refs/heads/one"), "ref: refs/heads/two\n").unwrap();
        std::fs::write(dir.join("refs/heads/two"), "ref: refs/heads/one\n").unwrap();
        assert_eq!(read_head_commit(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_head_commit_falls_back_to_packed_refs() {
        let dir = temp_dir("head-packed");
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            dir.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted\n\
             deadbeefcafe0123456789abcdef0123456789ab refs/heads/main\n",
        )
        .unwrap();
        assert_eq!(
            read_head_commit(&dir),
            Some("deadbeefcafe0123456789abcdef0123456789ab".to_string())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_head_commit_detached_head_is_the_sha_itself() {
        let dir = temp_dir("head-detached");
        std::fs::write(dir.join("HEAD"), "abc123def456\n").unwrap();
        assert_eq!(read_head_commit(&dir), Some("abc123def456".to_string()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_head_commit_unborn_branch_is_none() {
        let dir = temp_dir("head-unborn");
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(read_head_commit(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn git_state_detects_each_marker() {
        let dir = temp_dir("state-clean");
        assert_eq!(git_state(&dir), "");
        std::fs::remove_dir_all(&dir).unwrap();

        let dir = temp_dir("state-rebase");
        std::fs::create_dir_all(dir.join("rebase-merge")).unwrap();
        assert_eq!(git_state(&dir), "REBASING");
        std::fs::remove_dir_all(&dir).unwrap();

        let dir = temp_dir("state-merge");
        std::fs::write(dir.join("MERGE_HEAD"), "abc\n").unwrap();
        assert_eq!(git_state(&dir), "MERGING");
        std::fs::remove_dir_all(&dir).unwrap();

        let dir = temp_dir("state-cherry");
        std::fs::write(dir.join("CHERRY_PICK_HEAD"), "abc\n").unwrap();
        assert_eq!(git_state(&dir), "CHERRY-PICKING");
        std::fs::remove_dir_all(&dir).unwrap();

        let dir = temp_dir("state-bisect");
        std::fs::write(dir.join("BISECT_LOG"), "abc\n").unwrap();
        assert_eq!(git_state(&dir), "BISECTING");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn git_stash_count_reads_reflog_lines() {
        let dir = temp_dir("stash-some");
        std::fs::create_dir_all(dir.join("logs/refs")).unwrap();
        std::fs::write(dir.join("logs/refs/stash"), "line one\nline two\n").unwrap();
        assert_eq!(git_stash_count(&dir), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn git_stash_count_missing_file_is_zero() {
        let dir = temp_dir("stash-none");
        assert_eq!(git_stash_count(&dir), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_porcelain_v2_reads_ahead_behind_and_status_counts() {
        let out = b"# branch.oid abc123\n\
                     # branch.head main\n\
                     # branch.upstream origin/main\n\
                     # branch.ab +3 -2\n\
                     1 M. N... 100644 100644 100644 aaa bbb staged.txt\n\
                     1 .M N... 100644 100644 100644 aaa bbb modified.txt\n\
                     1 MM N... 100644 100644 100644 aaa bbb both.txt\n\
                     u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.txt\n\
                     ? untracked.txt\n";
        let counts = parse_porcelain_v2(out);
        assert_eq!(
            counts,
            GitCounts {
                ahead: 3,
                behind: 2,
                staged: 2,
                modified: 2,
                untracked: 1,
                conflicted: 1,
                dirty: 5,
            }
        );
    }

    #[test]
    fn parse_porcelain_v2_no_upstream_has_zero_ahead_behind() {
        let out = b"# branch.oid abc123\n# branch.head main\n";
        let counts = parse_porcelain_v2(out);
        assert_eq!(counts.ahead, 0);
        assert_eq!(counts.behind, 0);
    }

    #[test]
    fn portable_system_builtins_return_values() {
        assert_ne!(hostname().unwrap(), "");
        assert_eq!(load_avg().unwrap().split(' ').count(), 3);
        assert!(uptime_secs().unwrap() > 0);
        assert!(mem_used_percent().unwrap() <= 100);
    }

    #[test]
    fn parse_meminfo_used_percent_computes_from_total_and_available() {
        let meminfo =
            "MemTotal:       10000 kB\nMemFree:         1000 kB\nMemAvailable:    4000 kB\n";
        assert_eq!(parse_meminfo_used_percent(meminfo), Some(60));
        assert_eq!(parse_meminfo_used_percent("garbage"), None);
    }

    #[test]
    fn vm_used_percent_counts_app_wired_and_compressed_not_purgeable() {
        let pages = VmPages {
            internal: 500,
            purgeable: 100,
            wired: 150,
            compressed: 50,
        };
        // (500 - 100 + 150 + 50) pages * 4 KiB over 4000 KiB.
        assert_eq!(vm_used_percent(pages, 4096, 4_096_000), Some(60));
        // Never above 100, and a zero total is an error, not a division by zero.
        assert_eq!(vm_used_percent(pages, 4096, 4096), Some(100));
        assert_eq!(vm_used_percent(pages, 4096, 0), None);
    }

    const PMSET_ON_BATTERY: &str = "Now drawing from 'Battery Power'\n \
        -InternalBattery-0 (id=4653155)\t85%; discharging; 4:12 remaining present: true\n";
    const PMSET_AC_WITH_UPS: &str = "Now drawing from 'AC Power'\n \
        -Back-UPS ES 700 (id=1234)\t100%; charged; 0:00 remaining present: true\n \
        -InternalBattery-0 (id=4653155)\t7%; charging; (no estimate) present: true\n";

    #[test]
    fn pmset_on_ac_reads_the_drawing_from_header() {
        assert!(!pmset_on_ac(PMSET_ON_BATTERY));
        assert!(pmset_on_ac(PMSET_AC_WITH_UPS));
        assert!(!pmset_on_ac("Now drawing from 'UPS Power'\n"));
        // Unparseable output is treated as AC, like a desktop with no supplies.
        assert!(pmset_on_ac(""));
    }

    #[test]
    fn pmset_battery_percent_picks_internal_battery_or_named_device() {
        assert_eq!(
            pmset_battery_percent(PMSET_ON_BATTERY, None),
            Some("85".to_string())
        );
        // Default skips a UPS listed first; `device` selects by the printed name.
        assert_eq!(
            pmset_battery_percent(PMSET_AC_WITH_UPS, None),
            Some("7".to_string())
        );
        assert_eq!(
            pmset_battery_percent(PMSET_AC_WITH_UPS, Some("Back-UPS ES 700")),
            Some("100".to_string())
        );
        assert_eq!(
            pmset_battery_percent(PMSET_AC_WITH_UPS, Some("InternalBattery-1")),
            None
        );
        // Desktop Mac: header only.
        assert_eq!(
            pmset_battery_percent("Now drawing from 'AC Power'\n", None),
            None
        );
    }

    #[tokio::test]
    async fn run_with_timeout_kills_hanging_command() {
        let shared = Shared::new();
        let deadline = Instant::now() + Duration::from_millis(100);
        let args = ["0.5".to_string()];
        let result = run_with_timeout(
            "sleep",
            &args,
            None,
            deadline,
            1024,
            RunOpts {
                envs: NO_ENV,
                shared: &shared,
            },
        )
        .await;
        assert!(matches!(result, Err(ProviderError::Timeout)));
        assert!(shared.inflight.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_with_timeout_caps_stdout() {
        let shared = Shared::new();
        let deadline = Instant::now() + Duration::from_millis(500);
        let args = ["-c".to_string(), "yes | head -c 100000".to_string()];
        let result = run_with_timeout(
            "sh",
            &args,
            None,
            deadline,
            1024,
            RunOpts {
                envs: NO_ENV,
                shared: &shared,
            },
        )
        .await;
        assert!(matches!(result, Err(ProviderError::TooLarge)));
    }

    #[tokio::test]
    async fn stopping_closes_admission_for_queued_fetches_and_direct_spawns() {
        let shared = Shared::new();
        let permits = shared.semaphore.acquire_many(4).await.unwrap();
        let owned = shared.clone();
        let queued = tokio::spawn(async move {
            let badge = BadgeConfig {
                source: Source::Command {
                    command: "true".into(),
                    args: None,
                },
                extract: None,
                format: "{value}".into(),
                interval: Duration::from_secs(60),
                battery_interval: Duration::from_secs(60),
                active_window: Duration::from_secs(300),
                timeout: Duration::from_secs(2),
                max_output: 1024,
                scope: crate::config::Scope::Global,
            };
            fetch(&owned, &badge, None).await
        });
        tokio::task::yield_now().await;
        shared.stop();
        drop(permits);
        assert!(matches!(queued.await.unwrap(), Err(ProviderError::Stopped)));
        assert!(matches!(
            run_with_timeout(
                "true",
                &[],
                None,
                Instant::now() + Duration::from_secs(1),
                1024,
                RunOpts {
                    envs: NO_ENV,
                    shared: &shared,
                },
            )
            .await,
            Err(ProviderError::Stopped)
        ));
        assert!(shared.inflight.lock().unwrap().is_empty());
        assert!(shared.porcelain.lock().unwrap().is_empty());
    }

    /// Count of live processes whose argv is exactly `sleep <marker>` (`ps` works on Linux
    /// and macOS; zombies print as `[sleep] <defunct>`/`(sleep)`, so they never count).
    fn proc_cmdline_count(marker: &str) -> usize {
        let Ok(out) = std::process::Command::new("ps")
            .args(["-A", "-ww", "-o", "args="])
            .output()
        else {
            return 0;
        };
        let want = format!("sleep {marker}");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| line.trim() == want)
            .count()
    }

    #[tokio::test]
    async fn cancelling_a_process_future_releases_its_group_registration() {
        let shared = Shared::new();
        let owned = shared.clone();
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let marker = format!("{}.{}", 9_300_000 + std::process::id(), nanos % 1000);
        let running = tokio::spawn({
            let marker = marker.clone();
            async move {
                run_with_timeout(
                    "sh",
                    &["-c".into(), format!("sleep {marker} | sleep {marker}")],
                    None,
                    Instant::now() + Duration::from_secs(20),
                    1024,
                    RunOpts {
                        envs: NO_ENV,
                        shared: &owned,
                    },
                )
                .await
            }
        });
        let started = tokio::time::timeout(Duration::from_secs(2), async {
            while shared.inflight.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let both_stages_started = tokio::time::timeout(Duration::from_secs(2), async {
            while proc_cmdline_count(&marker) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        // Always abort the task, including a failed start, to avoid leaving the test's
        // process alive if an assertion fails.
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        assert!(started.is_ok(), "process never registered");
        assert!(
            both_stages_started.is_ok(),
            "pipeline never started both stages"
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !shared.inflight.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(shared.inflight.lock().unwrap().is_empty());
        tokio::time::timeout(Duration::from_secs(2), async {
            while proc_cmdline_count(&marker) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn git_branch_from_head_reads_branch_name() {
        let dir = temp_dir("branch-named");
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(git_branch_from_head(&dir), "feature/x");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn git_branch_from_head_detached_is_empty() {
        let dir = temp_dir("branch-detached");
        std::fs::write(dir.join("HEAD"), "abc123def456\n").unwrap();
        assert_eq!(git_branch_from_head(&dir), "");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn git_branch_from_head_ref_outside_heads_is_empty() {
        let dir = temp_dir("branch-other-ref");
        std::fs::write(dir.join("HEAD"), "ref: refs/tags/v1\n").unwrap();
        assert_eq!(git_branch_from_head(&dir), "");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_in_process_git_branch_resolves_through_worktree_gitdir() {
        let root = temp_dir("inprocess-worktree");
        let real_dir = root.join("elsewhere/.git/worktrees/wt");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", real_dir.display()),
        )
        .unwrap();
        std::fs::write(real_dir.join("HEAD"), "ref: refs/heads/br1\n").unwrap();
        assert_eq!(
            run_in_process_git(Builtin::GitBranch, git_dir(&root).as_deref()),
            Some(b"br1".to_vec())
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn run_in_process_git_outside_repo_is_empty_not_none() {
        assert_eq!(
            run_in_process_git(Builtin::GitBranch, git_dir(Path::new("/")).as_deref()),
            Some(Vec::new())
        );
    }

    #[test]
    fn run_in_process_git_non_file_builtin_is_none() {
        assert_eq!(run_in_process_git(Builtin::Hostname, None), None);
    }

    #[test]
    fn git_fingerprint_changes_when_head_is_rewritten() {
        let dir = temp_dir("fingerprint-head");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let before = git_fingerprint(&dir.join(".git"));
        // Guarantee a distinct mtime on filesystems with coarse (e.g. 1s) resolution.
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/other\n").unwrap();
        let after = git_fingerprint(&dir.join(".git"));
        assert_ne!(before, after);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn git_fingerprint_without_head_or_index_is_default() {
        assert_eq!(git_fingerprint(Path::new("/")), GitFingerprint::default());
    }

    #[tokio::test]
    async fn porcelain_fetch_refetches_after_completion_for_refresh_and_worktree_changes() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let shared = Shared::new();
        let root = temp_dir("porcelain-refresh");
        let calls = Arc::new(AtomicUsize::new(0));
        for output in [
            b"old worktree status".to_vec(),
            b"new worktree status".to_vec(),
        ] {
            let calls = Arc::clone(&calls);
            let output = output.clone();
            let expected = output.clone();
            let out = porcelain_fetch_with(
                &shared,
                root.clone(),
                Instant::now() + Duration::from_secs(2),
                65536,
                move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(output)
                },
            )
            .await
            .unwrap();
            assert_eq!(out.as_ref(), expected.as_slice());
            assert!(shared.porcelain.lock().unwrap().is_empty());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn porcelain_fetch_observes_worktree_changes_after_a_completed_status() {
        let root = temp_dir("porcelain-worktree-change");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        let shared = Shared::new();
        let first = porcelain_fetch(
            &shared,
            &root,
            Instant::now() + Duration::from_secs(5),
            65536,
        )
        .await
        .unwrap();
        assert_eq!(parse_porcelain_v2(first.as_ref()).untracked, 0);

        std::fs::write(root.join("new-file"), "changed after first status").unwrap();
        let second = porcelain_fetch(
            &shared,
            &root,
            Instant::now() + Duration::from_secs(5),
            65536,
        )
        .await
        .unwrap();
        assert_eq!(parse_porcelain_v2(second.as_ref()).untracked, 1);
        assert!(shared.porcelain.lock().unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn porcelain_fetch_concurrent_callers_share_initializer_but_apply_caps_independently() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let shared = Shared::new();
        let root = temp_dir("porcelain-limits");
        let calls = Arc::new(AtomicUsize::new(0));
        let deadline = Instant::now() + Duration::from_secs(2);
        let (large, small) = tokio::join!(
            {
                let calls = Arc::clone(&calls);
                porcelain_fetch_with(&shared, root.clone(), deadline, 16, move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(b"status".to_vec())
                })
            },
            {
                let calls = Arc::clone(&calls);
                porcelain_fetch_with(&shared, root.clone(), deadline, 2, move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(b"status".to_vec())
                })
            }
        );
        assert!(matches!(small, Err(ProviderError::TooLarge)));
        assert_eq!(large.unwrap().as_ref(), b"status");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(shared.porcelain.lock().unwrap().is_empty());

        let tighter_creator_root = root.join("tighter-creator");
        let (small_creator, large_joiner) = tokio::join!(
            {
                let calls = Arc::clone(&calls);
                porcelain_fetch_with(
                    &shared,
                    tighter_creator_root.clone(),
                    deadline,
                    2,
                    move || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Ok(b"status".to_vec())
                    },
                )
            },
            {
                let calls = Arc::clone(&calls);
                porcelain_fetch_with(
                    &shared,
                    tighter_creator_root,
                    deadline,
                    16,
                    move || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(b"unused".to_vec())
                    },
                )
            }
        );
        assert!(matches!(small_creator, Err(ProviderError::TooLarge)));
        assert!(matches!(large_joiner, Err(ProviderError::TooLarge)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(shared.porcelain.lock().unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn porcelain_fetch_callers_enforce_timeouts_without_cancelling_shared_work() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let shared = Shared::new();
        let root = temp_dir("porcelain-timeouts");
        let calls = Arc::new(AtomicUsize::new(0));
        let (long, short) = tokio::join!(
            {
                let calls = Arc::clone(&calls);
                porcelain_fetch_with(
                    &shared,
                    root.clone(),
                    Instant::now() + Duration::from_secs(2),
                    64,
                    move || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        Ok(b"finished".to_vec())
                    },
                )
            },
            {
                let calls = Arc::clone(&calls);
                porcelain_fetch_with(
                    &shared,
                    root.clone(),
                    Instant::now() + Duration::from_millis(10),
                    64,
                    move || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(b"unused second initializer".to_vec())
                    },
                )
            }
        );
        assert_eq!(long.unwrap().as_ref(), b"finished");
        assert!(matches!(short, Err(ProviderError::Timeout)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(shared.porcelain.lock().unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn porcelain_fetch_removes_failed_flights() {
        let shared = Shared::new();
        let root = temp_dir("porcelain-failure");
        let result = porcelain_fetch_with(
            &shared,
            root.clone(),
            Instant::now() + Duration::from_secs(2),
            64,
            || async { Err(ProviderError::Io("expected failure".to_string())) },
        )
        .await;
        assert!(matches!(result, Err(ProviderError::Io(_))));
        assert!(shared.porcelain.lock().unwrap().is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn porcelain_fetch_joins_before_semaphore_admission() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let shared = Shared::new();
        let root = temp_dir("porcelain-semaphore-share");
        let reserved = [
            shared.semaphore.acquire().await.unwrap(),
            shared.semaphore.acquire().await.unwrap(),
            shared.semaphore.acquire().await.unwrap(),
        ];
        let calls = Arc::new(AtomicUsize::new(0));
        let deadline = Instant::now() + Duration::from_secs(2);
        let (first, second, third) = tokio::join!(
            {
                let calls = Arc::clone(&calls);
                porcelain_fetch_with(&shared, root.clone(), deadline, 64, move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(b"shared".to_vec())
                })
            },
            {
                let calls = Arc::clone(&calls);
                porcelain_fetch_with(&shared, root.clone(), deadline, 64, move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(b"not used".to_vec())
                })
            },
            {
                let calls = Arc::clone(&calls);
                porcelain_fetch_with(&shared, root.clone(), deadline, 64, move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(b"not used".to_vec())
                })
            }
        );
        assert_eq!(first.unwrap().as_ref(), b"shared");
        assert_eq!(second.unwrap().as_ref(), b"shared");
        assert_eq!(third.unwrap().as_ref(), b"shared");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(shared.porcelain.lock().unwrap().is_empty());
        drop(reserved);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn porcelain_fetch_cleanup_does_not_remove_a_replacement_generation() {
        let shared = Shared::new();
        let root = temp_dir("porcelain-generation");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let replacement = {
            let (sender, _) = watch::channel(None);
            Arc::new(PorcelainFlight { result: sender })
        };
        let shared_ref = &shared;
        let root_for_fetch = root.clone();
        let (result, ()) = tokio::join!(
            porcelain_fetch_with(
                shared_ref,
                root.clone(),
                Instant::now() + Duration::from_secs(2),
                64,
                move || async move {
                    let _ = started_tx.send(());
                    let _ = finish_rx.await;
                    Ok(b"old generation".to_vec())
                },
            ),
            async {
                started_rx.await.unwrap();
                shared
                    .porcelain
                    .lock()
                    .unwrap()
                    .insert(root_for_fetch, Arc::clone(&replacement));
                finish_tx.send(()).unwrap();
            }
        );
        assert_eq!(result.unwrap().as_ref(), b"old generation");
        assert!(Arc::ptr_eq(
            &replacement,
            &shared.porcelain.lock().unwrap()[&root]
        ));
        shared.porcelain.lock().unwrap().remove(&root);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
