//! Daemon lifecycle: setsid/chdir, flock singleton, accept loop and dispatch, refresh
//! scheduling (`refresh_due`, `spawn_fetch`), idle exit. Submodules: `cache`, `handler`,
//! `reload`.

//! Scheduling is one loop: `select! { accept, sleep_until(next_due), signals }`. No
//! per-badge tasks, no fixed tick; refreshes are demand-driven and coalesced (see
//! `cache.rs`).
//!
//! The overall idea — one daemon that computes statusline values once and serves them from
//! a cache to every prompt/tmux caller over a Unix socket — comes from
//! [beachcomber](https://github.com/NavistAu/beachcomber); see `THIRD_PARTY_NOTICES.md`.

mod cache;
mod handler;
mod reload;

use std::collections::{BTreeMap, HashMap};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Notify, oneshot};

use crate::config::Config;
use crate::{ipc, provider};
use handler::handle_connection;
use reload::{Stamp, file_id};

struct ConfigMeta {
    path: PathBuf,
    /// Config file + `palette_from` file identities; a change in either triggers a reload.
    stamp: Stamp,
    last_checked: Instant,
    checking: bool,
    /// Last config load/reload failure, cleared by the next success; shown by `stfgd status`
    /// (the daemon's stderr is /dev/null).
    last_error: Option<String>,
}

/// Records a config error, logging it to stderr unless it repeats the previous one.
fn set_config_error(meta: &mut ConfigMeta, msg: String) {
    if meta.last_error.as_deref() != Some(&msg) {
        eprintln!("stfgd: {msg}");
    }
    meta.last_error = Some(msg);
}

struct OnAc {
    value: bool,
    checked_at: Instant,
}

/// Lock order (never held across an `.await`): `config_meta` -> `config` -> `cache`. Take
/// them in that order, skipping any you don't need. `last_request`, `last_evict` and `on_ac`
/// are leaf locks: take them alone, never while holding another lock (read what you need
/// from `config` first, via [`Daemon::config`]).
struct Daemon {
    cache: Mutex<HashMap<cache::Key, cache::Entry>>,
    config: Mutex<Arc<Config>>,
    config_meta: Mutex<ConfigMeta>,
    providers: provider::Shared,
    last_request: Mutex<Instant>,
    last_evict: Mutex<Instant>,
    on_ac: Mutex<OnAc>,
    shutdown: Notify,
    reload_finished: Notify,
    /// Signalled when a fetch finishes, so the loop recomputes `next_due` for that key.
    reschedule: Notify,
}

impl Daemon {
    fn new(
        config: Config,
        config_path: PathBuf,
        stamp: Stamp,
        providers: provider::Shared,
        on_ac: OnAc,
        now: Instant,
    ) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            config: Mutex::new(Arc::new(config)),
            config_meta: Mutex::new(ConfigMeta {
                path: config_path,
                stamp,
                last_checked: now,
                checking: false,
                last_error: None,
            }),
            providers,
            last_request: Mutex::new(now),
            last_evict: Mutex::new(now),
            on_ac: Mutex::new(on_ac),
            shutdown: Notify::new(),
            reload_finished: Notify::new(),
            reschedule: Notify::new(),
        }
    }

    /// Snapshot of the current config; the lock is released before returning.
    fn config(&self) -> Arc<Config> {
        Arc::clone(&self.config.lock().unwrap())
    }
}

/// Config/power maintenance is off the prompt path and must still finish promptly.
const MAINTENANCE_BUDGET: Duration = Duration::from_millis(500);

/// Pause after `accept` fails for lack of fds/memory (see the accept arm in `async_main`).
const ACCEPT_RETRY: Duration = Duration::from_millis(20);

/// Longest the loop sleeps without running its timer arm, which refreshes the lock file's
/// timestamps (see there). Prompts keep pushing the idle deadline out, and path-scoped-only
/// configs have no `next_due`, so without a cap that arm might never run.
const MAX_SLEEP: Duration = Duration::from_secs(3600);

/// Entry point for `stfgd daemon`. Never returns except by process exit.
pub fn run() {
    // First thing, always: new session (drops any controlling terminal, so SIGHUP from a
    // closing terminal never reaches us) and a cwd that doesn't pin mount points.
    // SAFETY: setsid has no preconditions; it only fails (EPERM) for a process-group
    // leader, which the client's spawn deliberately avoids (see `client.rs`'s `try_spawn_daemon`).
    unsafe { libc::setsid() };
    let _ = std::env::set_current_dir("/");

    if !prepare_runtime_dir(&ipc::runtime_dir()) {
        return;
    }

    let lock_path = ipc::lock_path();
    let Ok(lock_file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
    else {
        return;
    };
    // SAFETY: flock on an fd we own, no preconditions beyond a valid fd.
    let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return; // another daemon already holds the lock
    }

    // Only now that we hold the lock: any socket left behind is stale (its daemon died;
    // the kernel released the lock). Safe to unlink before binding.
    let sock_path = ipc::socket_path();
    let _ = std::fs::remove_file(&sock_path);

    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
    else {
        return;
    };
    rt.block_on(async_main(sock_path, &lock_file));
    // spawn_blocking cannot cancel a stuck filesystem syscall. Do not wait for such
    // workers on shutdown; returning from main terminates the daemon process.
    rt.shutdown_timeout(Duration::ZERO);
    drop(lock_file); // releases the flock; the kernel would anyway on exit
}

/// Creates the runtime dir `0700` and refuses one someone else owns or can write to (the
/// client refuses it too). On macOS this is normally the shared-`/tmp` fallback
/// (`XDG_RUNTIME_DIR` is unset there), where anyone can pre-create the path or plant a
/// symlink to capture or block our socket.
fn prepare_runtime_dir(dir: &Path) -> bool {
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir);
    // chmod(2) follows symlinks: only tighten a real dir we already own. Sticky `/tmp`
    // stops anyone else from swapping it for a symlink after this lstat.
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if std::fs::symlink_metadata(dir).is_ok_and(|md| md.is_dir() && md.uid() == uid) {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    matches!(ipc::runtime_dir_is_private(dir), Ok(true))
}

async fn async_main(sock_path: PathBuf, lock_file: &std::fs::File) {
    let config_path = ipc::config_path();
    let providers = provider::Shared::new();
    let path = config_path.clone();
    let loaded = providers
        .blocking
        .run(
            tokio::time::Instant::now() + MAINTENANCE_BUDGET,
            move || {
                let config_id = file_id(&path);
                let cfg = Config::load(&path);
                // A failed load leaves an empty stamp so the next check retries it.
                let st = cfg.as_ref().map_or_else(
                    |_| Stamp::default(),
                    |c| Stamp {
                        config: config_id,
                        palette: c.daemon.palette_from.as_deref().and_then(file_id),
                    },
                );
                (cfg, st)
            },
        )
        .await;
    let (loaded, stamp) = loaded.unwrap_or_else(|| {
        (
            Err("config read timed out or filesystem workers are busy".into()),
            Stamp::default(),
        )
    });
    let mut last_error = None;
    let initial = loaded.unwrap_or_else(|e| {
        last_error = Some(format!(
            "config load failed, started with an empty config: {e}"
        ));
        eprintln!("stfgd: {}", last_error.as_deref().unwrap_or_default());
        Config {
            daemon: crate::config::DaemonConfig::default(),
            badge: BTreeMap::new(),
            groups: BTreeMap::new(),
        }
    });
    let now = Instant::now();
    let power_checked_at = now.checked_sub(initial.daemon.power_check).unwrap_or(now);

    let state = Arc::new(Daemon::new(
        initial,
        config_path,
        stamp,
        providers,
        OnAc {
            value: true,
            checked_at: power_checked_at,
        },
        now,
    ));

    state.config_meta.lock().unwrap().last_error = last_error;

    let Ok(listener) = UnixListener::bind(&sock_path) else {
        return;
    };
    // Identity of the socket we bound, so shutdown never unlinks a successor's socket.
    let sock_id = std::fs::symlink_metadata(&sock_path)
        .ok()
        .map(|md| (md.dev(), md.ino()));
    let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
        return;
    };
    let Ok(mut sigint) = signal(SignalKind::interrupt()) else {
        return;
    };

    loop {
        let now = Instant::now();
        let cfg = state.config();
        let idle_exit = cfg.daemon.idle_exit;
        let idle_deadline = *state.last_request.lock().unwrap() + idle_exit;

        let on_ac = refresh_on_ac_if_due(&state, now);
        let next_due = cache::next_due(&state.cache.lock().unwrap(), &cfg.badge, on_ac, now);
        let target = next_due
            .map_or(idle_deadline, |t| t.min(idle_deadline))
            .min(now + MAX_SLEEP)
            .max(now);

        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(handle_connection(Arc::clone(&state), stream));
                }
                // These leave the connection queued and the listener readable, so an
                // immediate retry would spin; let in-flight work release resources.
                // macOS's default soft fd limit (256) makes EMFILE reachable.
                Err(e) if matches!(
                    e.raw_os_error(),
                    Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
                ) => tokio::time::sleep(ACCEPT_RETRY).await,
                Err(_) => {} // e.g. ECONNABORTED: that connection is gone, retry now
            },
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(target)) => {
                let now2 = Instant::now();
                let idle_for = now2.saturating_duration_since(*state.last_request.lock().unwrap());
                if idle_for >= idle_exit {
                    break;
                }
                // macOS's daily /tmp cleaner deletes files whose atime/mtime/ctime are all
                // older than 3 days, which would let a second daemon take a fresh lock
                // beside us. `MAX_SLEEP` makes this arm run at least hourly.
                let touched = SystemTime::now();
                let _ = lock_file.set_times(
                    std::fs::FileTimes::new()
                        .set_accessed(touched)
                        .set_modified(touched),
                );
                refresh_due(&state, now2);
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            () = state.shutdown.notified() => break,
            () = state.reschedule.notified() => {}
        }
    }

    state.providers.stop();
    let _ = state
        .providers
        .blocking
        .run(
            tokio::time::Instant::now() + MAINTENANCE_BUDGET,
            move || {
                // A successor may own this path by now (our lock file was deleted out from
                // under us); only unlink the socket we bound.
                let current = std::fs::symlink_metadata(&sock_path)
                    .ok()
                    .map(|md| (md.dev(), md.ino()));
                if current.is_some() && current == sock_id {
                    let _ = std::fs::remove_file(&sock_path);
                }
            },
        )
        .await;
}

/// Returns the last known AC state, and spawns a background probe to refresh it if the last
/// one is older than `power_check`.
fn refresh_on_ac_if_due(state: &Arc<Daemon>, now: Instant) -> bool {
    let power_check = state.config().daemon.power_check;
    let mut g = state.on_ac.lock().unwrap();
    if now.saturating_duration_since(g.checked_at) >= power_check {
        g.checked_at = now;
        let state = Arc::clone(state);
        tokio::spawn(async move {
            if let Some(value) = state
                .providers
                .blocking
                .run(
                    tokio::time::Instant::now() + MAINTENANCE_BUDGET,
                    provider::on_ac,
                )
                .await
            {
                state.on_ac.lock().unwrap().value = value;
                state.reschedule.notify_one();
            }
        });
    }
    g.value
}

/// Spawns one fetch per coalesced-due key, then evicts dormant/excess path-scoped keys.
fn refresh_due(state: &Arc<Daemon>, now: Instant) {
    let cfg = state.config();
    let on_ac = refresh_on_ac_if_due(state, now);
    let due = {
        let cache = state.cache.lock().unwrap();
        cache::due_keys(&cache, &cfg.badge, on_ac, cfg.daemon.coalesce, now)
    };
    for key in due {
        let mut cache = state.cache.lock().unwrap();
        let Some(entry) = cache.get_mut(&key) else {
            continue;
        };
        if entry.in_flight {
            continue;
        }
        entry.in_flight = true;
        let identity = Arc::clone(&entry.identity);
        drop(cache);
        spawn_fetch(Arc::clone(state), key, identity, Arc::clone(&cfg), None);
    }
    cache::evict(
        &mut state.cache.lock().unwrap(),
        cfg.daemon.path_evict,
        cfg.daemon.max_paths,
        now,
    );
}

/// Runs a fetch outside any lock, then writes the result back (single-flight release,
/// stale value kept on failure, backoff on failure).
fn spawn_fetch(
    state: Arc<Daemon>,
    key: cache::Key,
    identity: Arc<()>,
    cfg: Arc<Config>,
    done: Option<oneshot::Sender<()>>,
) {
    tokio::spawn(async move {
        let cwd = key.root.clone();
        let result = match cfg.badge.get(&key.badge) {
            Some(badge_cfg) => Some(
                provider::fetch(
                    &state.providers,
                    badge_cfg,
                    cwd.as_deref(),
                    key.env.as_deref(),
                )
                .await,
            ),
            None => None,
        };
        let now = Instant::now();
        {
            let current = state.config.lock().unwrap();
            let mut cache = state.cache.lock().unwrap();
            // A pre-reload fetch must not overwrite a value fetched under the new
            // configuration, or release that new fetch's single-flight flag.
            if Arc::ptr_eq(&current, &cfg)
                && let Some(entry) = cache.get_mut(&key)
                && Arc::ptr_eq(&entry.identity, &identity)
            {
                entry.in_flight = false;
                match result {
                    Some(Ok(rendered)) => {
                        entry.value = Some(rendered);
                        entry.fetched_at = Some(now);
                        entry.errors = 0;
                        entry.next_attempt = now;
                    }
                    Some(Err(_)) => {
                        entry.errors += 1;
                        entry.next_attempt = now
                            + cache::backoff(
                                cfg.daemon.retry_min,
                                cfg.daemon.retry_max,
                                entry.errors,
                            );
                    }
                    None => {}
                }
            }
        }
        state.reschedule.notify_one();
        if let Some(tx) = done {
            let _ = tx.send(());
        }
    });
}

/// Runs the path-scoped idle-eviction pass at most once every `config_check`, the same
/// throttle `check_reload` uses. The `max_paths` LRU cap itself is enforced immediately on
/// insert (see `handle_get`); this only needs to catch keys going idle, which is cheap to
/// delay.
fn maybe_evict_idle(state: &Daemon, cfg: &Config, now: Instant) {
    let mut last = state.last_evict.lock().unwrap();
    if now.saturating_duration_since(*last) < cfg.daemon.config_check {
        return;
    }
    *last = now;
    drop(last);
    cache::evict_idle(&mut state.cache.lock().unwrap(), cfg.daemon.path_evict, now);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BadgeConfig, DaemonConfig};

    pub(super) fn test_cfg(interval: Duration, battery_interval: Duration) -> Config {
        let mut badge = BTreeMap::new();
        badge.insert(
            "b".to_string(),
            BadgeConfig {
                interval,
                battery_interval,
                ..crate::config::test_badge()
            },
        );
        Config {
            daemon: DaemonConfig {
                power_check: Duration::from_secs(3600), // never re-probes mid-test
                cold_wait: Duration::from_millis(500),
                ..DaemonConfig::default()
            },
            badge,
            groups: BTreeMap::new(),
        }
    }

    pub(super) fn test_daemon(on_ac: bool) -> Arc<Daemon> {
        let now = Instant::now();
        Arc::new(Daemon::new(
            Config {
                daemon: DaemonConfig::default(),
                badge: BTreeMap::new(),
                groups: BTreeMap::new(),
            },
            PathBuf::new(),
            Stamp::default(),
            provider::Shared::new(),
            OnAc {
                value: on_ac,
                checked_at: now,
            },
            now,
        ))
    }

    #[tokio::test]
    async fn old_config_fetch_cannot_overwrite_a_new_config_entry() {
        let state = test_daemon(true);
        let old_cfg = Arc::new(test_cfg(Duration::from_secs(60), Duration::from_secs(60)));
        let key = cache::Key::global("b");
        let mut entry = cache::Entry::new(Instant::now());
        entry.value = Some("new-config-value".into());
        entry.in_flight = true;
        let identity = Arc::clone(&entry.identity);
        state.cache.lock().unwrap().insert(key.clone(), entry);
        let (tx, rx) = oneshot::channel();
        spawn_fetch(Arc::clone(&state), key.clone(), identity, old_cfg, Some(tx));
        tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();
        let cache = state.cache.lock().unwrap();
        assert_eq!(cache[&key].value.as_deref(), Some("new-config-value"));
        assert!(cache[&key].in_flight, "must not release a new fetch's flag");
        drop(cache);
    }

    #[tokio::test]
    async fn evicted_fetch_cannot_overwrite_a_replacement_entry() {
        let state = test_daemon(true);
        let cfg = Arc::new(test_cfg(Duration::from_secs(60), Duration::from_secs(60)));
        *state.config.lock().unwrap() = Arc::clone(&cfg);
        let key = cache::Key::global("b");
        let old_identity = cache::Entry::new(Instant::now()).identity;
        let mut replacement = cache::Entry::new(Instant::now());
        replacement.value = Some("replacement-value".into());
        replacement.in_flight = true;
        state.cache.lock().unwrap().insert(key.clone(), replacement);
        let (tx, rx) = oneshot::channel();
        spawn_fetch(Arc::clone(&state), key.clone(), old_identity, cfg, Some(tx));
        tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();
        let cache = state.cache.lock().unwrap();
        assert_eq!(cache[&key].value.as_deref(), Some("replacement-value"));
        assert!(
            cache[&key].in_flight,
            "must not release a replacement fetch"
        );
        drop(cache);
    }
}
