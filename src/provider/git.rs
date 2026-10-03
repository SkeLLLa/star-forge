//! Git builtins: repo discovery, in-process reads, and the shared porcelain fetch.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tokio::sync::{OwnedSemaphorePermit, watch};
use tokio::time::Instant;

use super::exec::{RunOpts, run_with_timeout};
use super::{ProviderError, Shared};
use crate::config::{Builtin, GitField};

const GIT_ENV: &[(&str, &str)] = &[("GIT_OPTIONAL_LOCKS", "0")];

/// A `git status` result shared only while its subprocess is running. The sender stores the
/// completed result long enough for current waiters to receive it, but the map entry is
/// removed before publication so a later badge refresh always starts a fresh status check.
pub(super) struct PorcelainFlight {
    result: watch::Sender<Option<Result<Arc<[u8]>, ProviderError>>>,
}

// Adapted from beachcomber `src/provider/git.rs` `find_repo_root` (MIT, Copyright (c) 2026
// Joshua Hogendorn); see THIRD_PARTY_NOTICES.md.
/// Walks up from `start` looking for a `.git` entry (dir, or file for worktrees). In-process
/// `stat`s only, never a subprocess. `None` means `start` is not inside a repo.
pub(crate) fn git_root(start: &Path) -> Option<PathBuf> {
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

/// Nearest ancestor of `start` (itself included) containing any of `markers`, for
/// `scope = "project_root"`. The walk ends after `home` (when `start` is under it) or `/`, so a
/// stray marker in a parent of `$HOME` never matches. Plain `stat`s, like `git_root`.
pub(crate) fn project_root(
    start: &Path,
    markers: &[String],
    home: Option<&Path>,
) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if markers.iter().any(|m| dir.join(m).exists()) {
            return Some(dir);
        }
        if home == Some(dir.as_path()) || !dir.pop() {
            return None;
        }
    }
}

// Adapted from beachcomber `src/provider/git.rs` `resolve_git_dir` (MIT, Copyright (c) 2026
// Joshua Hogendorn); see THIRD_PARTY_NOTICES.md.
/// Resolves the actual git directory for `repo_root`: a `.git` directory (including a symlink
/// to one), or (for worktrees) the target of a `.git` *file*'s `gitdir: ...` line. In-process
/// only.
pub(crate) fn git_dir(repo_root: &Path) -> Option<PathBuf> {
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
/// Limitation: doesn't see unstaged worktree edits (they touch neither `HEAD` nor `index`), so
/// those still only surface on `interval` — documented in README; the upgrade path if that
/// ever matters enough is inotify on the worktree, at the cost of a watcher thread this
/// design otherwise avoids entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GitFingerprint {
    head: Option<SystemTime>,
    index: Option<SystemTime>,
}

pub(crate) fn git_fingerprint(dir: &Path) -> GitFingerprint {
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
/// Limitation: takes the literal first 7 chars as the "short" hash rather than git's
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
/// `daemon::handler::handle_get`, design.md §6). `Some(_)` (possibly empty bytes — outside a repo,
/// detached HEAD, etc.) for these four builtins; `None` for everything else, which still
/// goes through the normal cache + (for `git_status`/`git_counts`) subprocess path.
pub(crate) fn run_in_process_git(name: Builtin, git_dir: Option<&Path>) -> Option<Vec<u8>> {
    let read: fn(&Path) -> Vec<u8> = match name {
        Builtin::GitBranch => |dir| git_branch_from_head(dir).into_bytes(),
        Builtin::GitCommit => |dir| {
            read_head_commit(dir)
                .map(|sha| sha.get(..7).unwrap_or(&sha).as_bytes().to_vec())
                .unwrap_or_default()
        },
        Builtin::GitState => |dir| git_state(dir).as_bytes().to_vec(),
        Builtin::GitStash => |dir| count_or_empty(git_stash_count(&common_git_dir(dir))),
        Builtin::GitStatus
        | Builtin::GitCounts
        | Builtin::Battery
        | Builtin::Hostname
        | Builtin::LoadAvg
        | Builtin::MemUsedPercent
        | Builtin::Uptime => return None,
    };
    Some(git_dir.map_or_else(Vec::new, read))
}

/// Status counts parsed from `git status --porcelain=v2 --branch`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct GitCounts {
    ahead: u32,
    behind: u32,
    staged: u32,
    modified: u32,
    untracked: u32,
    conflicted: u32,
    /// Every changed entry (`1`/`2`/`u`/`?` line), once each: what `git_status` reports.
    pub(super) dirty: u32,
}

// Adapted from beachcomber `src/provider/git.rs` `parse_git_status` (MIT, Copyright (c) 2026
// Joshua Hogendorn); see THIRD_PARTY_NOTICES.md.
/// Parses `git status --porcelain=v2 --branch` output. Header lines (`#`) carry
/// `branch.ab +<ahead> -<behind>` (absent with no upstream); `1`/`2` file lines carry a 2-char
/// `XY` status (staged = `X != '.'`, modified = `Y != '.'`); `u` lines are always conflicted;
/// `?` lines are untracked.
pub(super) fn parse_porcelain_v2(out: &[u8]) -> GitCounts {
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
pub(super) fn count_or_empty(n: u32) -> Vec<u8> {
    if n == 0 {
        Vec::new()
    } else {
        n.to_string().into_bytes()
    }
}

pub(super) fn git_field_value(counts: &GitCounts, field: GitField) -> Vec<u8> {
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
        match flights.entry(repo_root.clone()) {
            Entry::Occupied(entry) => (Arc::clone(entry.get()), false),
            Entry::Vacant(entry) => {
                let (sender, _) = watch::channel(None);
                let flight = Arc::new(PorcelainFlight { result: sender });
                entry.insert(Arc::clone(&flight));
                (flight, true)
            }
        }
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

pub(super) async fn porcelain_fetch(
    shared: &Shared,
    repo_root: &Path,
    deadline: Instant,
    cap: usize,
) -> Result<Arc<[u8]>, ProviderError> {
    let cwd = repo_root.to_path_buf();
    let owned = shared.clone();
    porcelain_fetch_with(shared, cwd.clone(), deadline, cap, move || async move {
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
                unset: &[],
                shared: &owned,
            },
        )
        .await
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn git_root_finds_dot_git_upward() {
        let tmp = TempDir::new("git-root");
        let dir = tmp.path().to_path_buf();
        let nested = dir.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert_eq!(git_root(&nested), Some(dir));
    }

    #[test]
    fn git_root_none_outside_repo() {
        assert_eq!(git_root(Path::new("/")), None);
    }

    #[test]
    fn project_root_picks_nearest_marker_and_stops_at_home() {
        let top_tmp = TempDir::new("proj-root");
        let top = top_tmp.path().to_path_buf();
        let sub = top.join("sub/deep");
        std::fs::create_dir_all(&sub).unwrap();
        let m = [".nvmrc".to_string(), "package.json".to_string()];
        // No marker anywhere under `home`: no match, even though `top`'s parents are walked
        // only up to `top`.
        assert_eq!(project_root(&sub, &m, Some(&top)), None);
        std::fs::write(top.join("package.json"), "").unwrap();
        assert_eq!(project_root(&sub, &m, Some(&top)), Some(top.clone()));
        // Nested marker wins over the outer one.
        std::fs::write(top.join("sub/.nvmrc"), "").unwrap();
        assert_eq!(project_root(&sub, &m, Some(&top)), Some(top.join("sub")));
        // A marker above `home` is never reached.
        let home = top.join("sub");
        std::fs::remove_file(top.join("sub/.nvmrc")).unwrap();
        assert_eq!(project_root(&sub, &m, Some(&home)), None);
    }

    #[test]
    fn git_dir_resolves_plain_repo() {
        let root_tmp = TempDir::new("gitdir-plain");
        let root = root_tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(git_dir(&root), Some(root.join(".git")));
    }

    #[test]
    fn git_dir_follows_absolute_dot_git_directory_symlink() {
        use std::os::unix::fs::symlink;

        let root_tmp = TempDir::new("gitdir-absolute-symlink");
        let root = root_tmp.path().to_path_buf();
        let target = root.join("actual-git-dir");
        let repo = root.join("repo");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        symlink(&target, repo.join(".git")).unwrap();
        assert_eq!(git_dir(&repo), Some(repo.join(".git")));
    }

    #[test]
    fn git_dir_follows_relative_dot_git_directory_symlink() {
        use std::os::unix::fs::symlink;

        let root_tmp = TempDir::new("gitdir-relative-symlink");
        let root = root_tmp.path().to_path_buf();
        let target = root.join("actual-git-dir");
        let repo = root.join("repo");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        symlink("../actual-git-dir", repo.join(".git")).unwrap();
        assert_eq!(git_dir(&repo), Some(repo.join(".git")));
    }

    #[test]
    fn git_dir_rejects_broken_dot_git_symlink() {
        use std::os::unix::fs::symlink;

        let root_tmp = TempDir::new("gitdir-broken-symlink");
        let root = root_tmp.path().to_path_buf();
        symlink("missing-git-dir", root.join(".git")).unwrap();
        assert_eq!(git_dir(&root), None);
    }

    #[test]
    fn git_dir_follows_worktree_gitdir_file() {
        let root_tmp = TempDir::new("gitdir-worktree");
        let root = root_tmp.path().to_path_buf();
        let real_dir = root.join("elsewhere/.git/worktrees/wt");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", real_dir.display()),
        )
        .unwrap();
        assert_eq!(git_dir(&root), Some(real_dir));
    }

    #[test]
    fn common_git_dir_follows_commondir_file() {
        let dir_tmp = TempDir::new("commondir");
        let dir = dir_tmp.path().to_path_buf();
        let main = dir.join("main-git");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::write(dir.join("commondir"), "../main-git\n").unwrap();
        assert_eq!(common_git_dir(&dir), dir.join("../main-git"));
    }

    #[test]
    fn common_git_dir_without_commondir_is_itself() {
        let dir_tmp = TempDir::new("no-commondir");
        let dir = dir_tmp.path().to_path_buf();
        assert_eq!(common_git_dir(&dir), dir.clone());
    }

    #[test]
    fn read_head_commit_resolves_loose_ref() {
        let dir_tmp = TempDir::new("head-loose");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(dir.join("refs/heads")).unwrap();
        std::fs::write(dir.join("refs/heads/main"), "abc123def456\n").unwrap();
        assert_eq!(read_head_commit(&dir), Some("abc123def456".to_string()));
    }

    #[test]
    fn read_head_commit_follows_symbolic_loose_ref_chain() {
        let dir_tmp = TempDir::new("head-symbolic-chain");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/alias\n").unwrap();
        std::fs::create_dir_all(dir.join("refs/heads")).unwrap();
        std::fs::write(dir.join("refs/heads/alias"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.join("refs/heads/main"), "abc123def456\n").unwrap();
        assert_eq!(read_head_commit(&dir), Some("abc123def456".to_string()));
    }

    #[test]
    fn read_head_commit_bounds_symbolic_ref_chains() {
        let dir_tmp = TempDir::new("head-symbolic-loop");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/one\n").unwrap();
        std::fs::create_dir_all(dir.join("refs/heads")).unwrap();
        std::fs::write(dir.join("refs/heads/one"), "ref: refs/heads/two\n").unwrap();
        std::fs::write(dir.join("refs/heads/two"), "ref: refs/heads/one\n").unwrap();
        assert_eq!(read_head_commit(&dir), None);
    }

    #[test]
    fn read_head_commit_falls_back_to_packed_refs() {
        let dir_tmp = TempDir::new("head-packed");
        let dir = dir_tmp.path().to_path_buf();
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
    }

    #[test]
    fn read_head_commit_detached_head_is_the_sha_itself() {
        let dir_tmp = TempDir::new("head-detached");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::write(dir.join("HEAD"), "abc123def456\n").unwrap();
        assert_eq!(read_head_commit(&dir), Some("abc123def456".to_string()));
    }

    #[test]
    fn read_head_commit_unborn_branch_is_none() {
        let dir_tmp = TempDir::new("head-unborn");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(read_head_commit(&dir), None);
    }

    #[test]
    fn git_state_detects_each_marker() {
        let dir_tmp = TempDir::new("state-clean");
        let dir = dir_tmp.path().to_path_buf();
        assert_eq!(git_state(&dir), "");

        let dir_tmp = TempDir::new("state-rebase");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::create_dir_all(dir.join("rebase-merge")).unwrap();
        assert_eq!(git_state(&dir), "REBASING");

        let dir_tmp = TempDir::new("state-merge");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::write(dir.join("MERGE_HEAD"), "abc\n").unwrap();
        assert_eq!(git_state(&dir), "MERGING");

        let dir_tmp = TempDir::new("state-cherry");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::write(dir.join("CHERRY_PICK_HEAD"), "abc\n").unwrap();
        assert_eq!(git_state(&dir), "CHERRY-PICKING");

        let dir_tmp = TempDir::new("state-bisect");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::write(dir.join("BISECT_LOG"), "abc\n").unwrap();
        assert_eq!(git_state(&dir), "BISECTING");
    }

    #[test]
    fn git_stash_count_reads_reflog_lines() {
        let dir_tmp = TempDir::new("stash-some");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::create_dir_all(dir.join("logs/refs")).unwrap();
        std::fs::write(dir.join("logs/refs/stash"), "line one\nline two\n").unwrap();
        assert_eq!(git_stash_count(&dir), 2);
    }

    #[test]
    fn git_stash_count_missing_file_is_zero() {
        let dir_tmp = TempDir::new("stash-none");
        let dir = dir_tmp.path().to_path_buf();
        assert_eq!(git_stash_count(&dir), 0);
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
    fn parse_porcelain_v2_counts_renames_and_conflicts_among_plain_entries() {
        let out = b"2 R. N... 100644 100644 100644 aaa bbb R100 new.txt\told.txt\n\
                     1 .M N... 100644 100644 100644 aaa bbb m.txt\n\
                     u UU N... 100644 100644 100644 100644 aaa bbb ccc c.txt\n\
                     2 RM N... 100644 100644 100644 aaa bbb R90 n2.txt\to2.txt\n";
        let counts = parse_porcelain_v2(out);
        assert_eq!(
            counts,
            GitCounts {
                staged: 2,
                modified: 2,
                conflicted: 1,
                dirty: 4,
                ..GitCounts::default()
            }
        );
    }

    #[test]
    fn parse_porcelain_v2_garbage_branch_ab_is_zero() {
        let counts = parse_porcelain_v2(b"# branch.ab nonsense here\n# branch.ab +x -\n");
        assert_eq!((counts.ahead, counts.behind), (0, 0));
    }

    #[test]
    fn git_branch_from_head_reads_branch_name() {
        let dir_tmp = TempDir::new("branch-named");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(git_branch_from_head(&dir), "feature/x");
    }

    #[test]
    fn git_branch_from_head_detached_is_empty() {
        let dir_tmp = TempDir::new("branch-detached");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::write(dir.join("HEAD"), "abc123def456\n").unwrap();
        assert_eq!(git_branch_from_head(&dir), "");
    }

    #[test]
    fn git_branch_from_head_ref_outside_heads_is_empty() {
        let dir_tmp = TempDir::new("branch-other-ref");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::write(dir.join("HEAD"), "ref: refs/tags/v1\n").unwrap();
        assert_eq!(git_branch_from_head(&dir), "");
    }

    #[test]
    fn run_in_process_git_branch_resolves_through_worktree_gitdir() {
        let root_tmp = TempDir::new("inprocess-worktree");
        let root = root_tmp.path().to_path_buf();
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
        let dir_tmp = TempDir::new("fingerprint-head");
        let dir = dir_tmp.path().to_path_buf();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let before = git_fingerprint(&dir.join(".git"));
        // Guarantee a distinct mtime on filesystems with coarse (e.g. 1s) resolution.
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/other\n").unwrap();
        let after = git_fingerprint(&dir.join(".git"));
        assert_ne!(before, after);
    }

    #[test]
    fn git_fingerprint_without_head_or_index_is_default() {
        assert_eq!(git_fingerprint(Path::new("/")), GitFingerprint::default());
    }

    #[tokio::test]
    async fn porcelain_fetch_refetches_after_completion_for_refresh_and_worktree_changes() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let shared = Shared::new();
        let root_tmp = TempDir::new("porcelain-refresh");
        let root = root_tmp.path().to_path_buf();
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
    }

    #[tokio::test]
    async fn porcelain_fetch_observes_worktree_changes_after_a_completed_status() {
        let root_tmp = TempDir::new("porcelain-worktree-change");
        let root = root_tmp.path().to_path_buf();
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
    }

    #[tokio::test]
    async fn porcelain_fetch_concurrent_callers_share_initializer_but_apply_caps_independently() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let shared = Shared::new();
        let root_tmp = TempDir::new("porcelain-limits");
        let root = root_tmp.path().to_path_buf();
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
    }

    #[tokio::test]
    async fn porcelain_fetch_callers_enforce_timeouts_without_cancelling_shared_work() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let shared = Shared::new();
        let root_tmp = TempDir::new("porcelain-timeouts");
        let root = root_tmp.path().to_path_buf();
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
    }

    #[tokio::test]
    async fn porcelain_fetch_removes_failed_flights() {
        let shared = Shared::new();
        let root_tmp = TempDir::new("porcelain-failure");
        let root = root_tmp.path().to_path_buf();
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
    }

    #[tokio::test]
    async fn porcelain_fetch_joins_before_semaphore_admission() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let shared = Shared::new();
        let root_tmp = TempDir::new("porcelain-semaphore-share");
        let root = root_tmp.path().to_path_buf();
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
    }

    #[tokio::test]
    async fn porcelain_fetch_cleanup_does_not_remove_a_replacement_generation() {
        let shared = Shared::new();
        let root_tmp = TempDir::new("porcelain-generation");
        let root = root_tmp.path().to_path_buf();
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
    }
}
