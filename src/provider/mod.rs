//! Subprocess/HTTP/builtin fetches, all bounded by a deadline and a byte cap.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::config::{BadgeConfig, Builtin, GitField, Source};
use crate::extract;

mod exec;
mod git;
mod system;

use exec::{RunOpts, http_fetch, run_with_timeout};
pub(crate) use git::{
    GitFingerprint, git_dir, git_fingerprint, git_root, project_root, run_in_process_git,
};
use git::{PorcelainFlight, count_or_empty, git_field_value, parse_porcelain_v2, porcelain_fetch};
pub use system::on_ac;
use system::{battery, hostname, load_avg, mem_used_percent, uptime_secs};

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

impl From<std::io::Error> for ProviderError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
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

/// State shared by every fetch: one pooled HTTP client, the in-flight process-group
/// registry (killed on stop/signal/idle-exit), a global concurrency cap, and the in-flight-only
/// per-repo `git_counts` singleflight map (see `porcelain_fetch`).
#[derive(Clone)]
pub struct Shared {
    http: reqwest::Client,
    inflight: Arc<Mutex<HashSet<i32>>>,
    semaphore: Arc<Semaphore>,
    pub blocking: crate::blocking::Pool,
    stopping: Arc<AtomicBool>,
    porcelain: Arc<Mutex<HashMap<PathBuf, Arc<PorcelainFlight>>>>,
}

impl Shared {
    pub(crate) fn new() -> Self {
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
    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.semaphore.close();
        let pgids = self.inflight.lock().unwrap();
        for &pgid in pgids.iter() {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
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
        // In-process (no subprocess, no fetch-path cache entry): `daemon::handler::handle_get`
        // intercepts these four via `git_snapshot` before calling `fetch`/`fetch_raw`. This arm
        // keeps dispatch total and serves direct `fetch` callers, off the reactor.
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
        // µs-scale syscalls that cannot hang: inline, no blocking-pool hop.
        Builtin::Hostname => hostname().map(String::into_bytes),
        Builtin::LoadAvg => load_avg().map(String::into_bytes),
        Builtin::Uptime => uptime_secs().map(|s| s.to_string().into_bytes()),
        Builtin::MemUsedPercent => shared
            .blocking
            .run(deadline, || {
                mem_used_percent().map(|p| p.to_string().into_bytes())
            })
            .await
            .ok_or(ProviderError::Timeout)?,
    }
}

async fn fetch_raw(
    shared: &Shared,
    source: &Source,
    cwd: Option<&Path>,
    env: Option<&[(String, String)]>,
    patterns: &[String],
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
            // The client sent its env: an allowlisted variable it lacks must not leak in
            // from the daemon's own.
            let unset: Vec<String> = env.map_or_else(Vec::new, |sel| {
                std::env::vars_os()
                    .filter_map(|(k, _)| k.into_string().ok())
                    .filter(|k| {
                        patterns.iter().any(|p| crate::config::env_matches(p, k))
                            && !sel.iter().any(|(n, _)| n == k)
                    })
                    .collect()
            });
            run_with_timeout(
                prog,
                &argv,
                cwd,
                deadline,
                cap,
                RunOpts {
                    envs: &env
                        .unwrap_or_default()
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect::<Vec<_>>(),
                    unset: &unset,
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
    env: Option<&[(String, String)]>,
) -> Result<String, ProviderError> {
    if shared.stopping.load(Ordering::SeqCst) {
        return Err(ProviderError::Stopped);
    }
    let deadline = Instant::now() + badge.timeout;
    let _permit = if matches!(
        badge.source,
        Source::Builtin { name, .. } if name.uses_porcelain()
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

    let raw = fetch_raw(
        shared,
        &badge.source,
        cwd,
        env,
        &badge.path.env,
        deadline,
        badge.max_output,
    )
    .await?;
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

    #[tokio::test]
    async fn stopping_closes_admission_for_queued_fetches_and_direct_spawns() {
        let shared = Shared::new();
        let permits = shared.semaphore.acquire_many(4).await.unwrap();
        let owned = shared.clone();
        let queued = tokio::spawn(async move {
            let badge = crate::config::test_badge();
            fetch(&owned, &badge, None, None).await
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
                    envs: &[],
                    unset: &[],
                    shared: &shared,
                },
            )
            .await,
            Err(ProviderError::Stopped)
        ));
        assert!(shared.inflight.lock().unwrap().is_empty());
        assert!(shared.porcelain.lock().unwrap().is_empty());
    }
}
