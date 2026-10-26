//! Daemon lifecycle: setsid/chdir, flock singleton, accept loop and dispatch, refresh
//! scheduling (`refresh_due`, `spawn_fetch`), idle exit. Submodules: `cache`, `protocol`
//! (wire format, connection dispatch), `resolve` (answering a `get`), `render` (response and
//! status text), `reload`, `upgrade`.

//! Scheduling is one loop: `select! { accept, sleep_until(next_due), signals }`. No
//! per-badge tasks, no fixed tick; refreshes are demand-driven and coalesced (see
//! `cache.rs`).
//!
//! The overall idea — one daemon that computes statusline values once and serves them from
//! a cache to every prompt/tmux caller over a Unix socket — comes from
//! [beachcomber](https://github.com/NavistAu/beachcomber); see `THIRD_PARTY_NOTICES.md`.

mod cache;
mod protocol;
mod reload;
mod render;
mod resolve;
mod upgrade;

use std::collections::{BTreeMap, HashMap};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Notify, oneshot};

use crate::config::Config;
use crate::{ipc, provider};
use protocol::handle_connection;
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
    /// Where the config's `env` patterns are published for clients (`None`: not published,
    /// as in tests).
    env_patterns: Option<PathBuf>,
}

/// Records a config error, logging it to stderr unless it repeats the previous one.
fn set_config_error(meta: &mut ConfigMeta, msg: String) {
    if meta.last_error.as_deref() != Some(&msg) {
        eprintln!("stfgd: {msg}");
    }
    meta.last_error = Some(msg);
}

/// Initial power reading for [`Daemon::new`].
#[derive(Clone, Copy)]
struct PowerState {
    value: cache::Power,
    checked_at: Instant,
}

/// An `Instant` in an atomic: nanoseconds since `epoch`, its initial value (earlier instants
/// saturate to it). Replaces what were leaf mutexes, so they need no lock-order rule.
struct AtomicInstant {
    epoch: Instant,
    nanos: AtomicU64,
}

impl AtomicInstant {
    const fn new(at: Instant) -> Self {
        Self {
            epoch: at,
            nanos: AtomicU64::new(0),
        }
    }

    fn nanos_of(&self, t: Instant) -> u64 {
        u64::try_from(t.saturating_duration_since(self.epoch).as_nanos()).unwrap_or(u64::MAX)
    }

    fn get(&self) -> Instant {
        self.epoch + Duration::from_nanos(self.nanos.load(Ordering::Relaxed))
    }

    fn set(&self, t: Instant) {
        self.nanos.store(self.nanos_of(t), Ordering::Relaxed);
    }

    /// Sets `now` and returns `true` if the stored instant is at least `gap` before it; of
    /// several callers in one window, only the first wins.
    fn claim_if_elapsed(&self, now: Instant, gap: Duration) -> bool {
        let now = self.nanos_of(now);
        let gap = u64::try_from(gap.as_nanos()).unwrap_or(u64::MAX);
        let last = self.nanos.load(Ordering::Relaxed);
        now.saturating_sub(last) >= gap
            && self
                .nanos
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }
}

/// The config and the cache it governs, under one lock: a reload swaps both at once, and a
/// fetch's write-back sees the config it is checked against, with no lock order to follow.
struct Core {
    config: Arc<Config>,
    cache: HashMap<cache::Key, cache::Entry>,
}

/// Two locks, `core` and `config_meta`, each taken alone (never both at once) and never held
/// across an `.await`; everything else is atomic or a `Notify`.
struct Daemon {
    core: Mutex<Core>,
    config_meta: Mutex<ConfigMeta>,
    providers: provider::Shared,
    last_request: AtomicInstant,
    last_evict: AtomicInstant,
    power_checked_at: AtomicInstant,
    on_battery: AtomicBool,
    shutdown: Notify,
    reload_finished: Notify,
    /// Signalled when a fetch finishes, so the loop recomputes `next_due` for that key.
    reschedule: Notify,
    upgrade: upgrade::Upgrade,
}

impl Daemon {
    fn new(
        config: Config,
        config_path: PathBuf,
        stamp: Stamp,
        providers: provider::Shared,
        power: PowerState,
        upgrade: upgrade::Upgrade,
        now: Instant,
    ) -> Self {
        Self {
            core: Mutex::new(Core {
                config: Arc::new(config),
                cache: HashMap::new(),
            }),
            config_meta: Mutex::new(ConfigMeta {
                path: config_path,
                stamp,
                last_checked: now,
                checking: false,
                last_error: None,
                env_patterns: None,
            }),
            providers,
            last_request: AtomicInstant::new(now),
            last_evict: AtomicInstant::new(now),
            power_checked_at: AtomicInstant::new(power.checked_at),
            on_battery: AtomicBool::new(power.value == cache::Power::Battery),
            shutdown: Notify::new(),
            reload_finished: Notify::new(),
            reschedule: Notify::new(),
            upgrade,
        }
    }

    fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().expect("core lock poisoned")
    }

    /// Snapshot of the current config; the lock is released before returning.
    fn config(&self) -> Arc<Config> {
        Arc::clone(&self.core().config)
    }

    fn power(&self) -> cache::Power {
        if self.on_battery.load(Ordering::Relaxed) {
            cache::Power::Battery
        } else {
            cache::Power::Ac
        }
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
    // Bind before loading the config (up to `MAINTENANCE_BUDGET`): a client that connects
    // meanwhile is queued, rather than seeing no socket and spawning yet another daemon.
    let Ok(listener) = std::os::unix::net::UnixListener::bind(&sock_path) else {
        return;
    };
    // Identity of the socket we bound, so shutdown never unlinks a successor's socket.
    let sock_id = std::fs::symlink_metadata(&sock_path)
        .ok()
        .map(|md| (md.dev(), md.ino()));

    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        // Both pools' slots, plus headroom for tokio's own blocking work (reqwest's DNS
        // lookups go through `spawn_blocking` too).
        .max_blocking_threads(provider::PATH_WORKERS + provider::GLOBAL_WORKERS + 8)
        .build()
    else {
        return;
    };
    let upgrade = upgrade::Upgrade::record(Instant::now());
    rt.block_on(async_main(
        upgrade,
        Bound {
            listener,
            path: sock_path,
            id: sock_id,
        },
        &lock_file,
    ));
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

/// The daemon socket, bound (not yet listening through tokio) before startup work.
struct Bound {
    listener: std::os::unix::net::UnixListener,
    path: PathBuf,
    /// `(dev, ino)` of the socket file we created.
    id: Option<(u64, u64)>,
}

async fn async_main(upgrade: upgrade::Upgrade, sock: Bound, lock_file: &std::fs::File) {
    let Bound {
        listener,
        path: sock_path,
        id: sock_id,
    } = sock;
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
    let (loaded, stamp) = loaded.unwrap_or_else(|why| {
        let why = match why {
            crate::blocking::Rejected::Saturated => "filesystem workers are busy",
            crate::blocking::Rejected::Expired => "config read timed out",
            crate::blocking::Rejected::Panicked => "config read panicked",
        };
        (Err(why.into()), Stamp::default())
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
        PowerState {
            value: cache::Power::Ac,
            checked_at: power_checked_at,
        },
        upgrade,
        now,
    ));

    {
        let mut meta = state.config_meta.lock().expect("config_meta lock poisoned");
        meta.last_error = last_error;
        meta.env_patterns = Some(ipc::env_patterns_path());
    }
    reload::publish_env_patterns(&state, state.config()).await;

    let Ok(listener) = listener
        .set_nonblocking(true)
        .and_then(|()| UnixListener::from_std(listener))
    else {
        return;
    };
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
        let idle_deadline = state.last_request.get() + idle_exit;

        let power = refresh_power_if_due(&state, now);
        let next_due = cache::next_due(&state.core().cache, &cfg.badge, power, now);
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
                let idle_for = now2.saturating_duration_since(state.last_request.get());
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
fn refresh_power_if_due(state: &Arc<Daemon>, now: Instant) -> cache::Power {
    let power_check = state.config().daemon.power_check;
    if state.power_checked_at.claim_if_elapsed(now, power_check) {
        let state = Arc::clone(state);
        tokio::spawn(async move {
            if let Ok(on_ac) = state
                .providers
                .blocking
                .run(
                    tokio::time::Instant::now() + MAINTENANCE_BUDGET,
                    provider::on_ac,
                )
                .await
            {
                state.on_battery.store(!on_ac, Ordering::Relaxed);
                state.reschedule.notify_one();
            }
        });
    }
    state.power()
}

/// Spawns one fetch per coalesced-due key, then evicts dormant/excess path-scoped keys.
fn refresh_due(state: &Arc<Daemon>, now: Instant) {
    let cfg = state.config();
    let power = refresh_power_if_due(state, now);
    let due = cache::due_keys(
        &state.core().cache,
        &cfg.badge,
        power,
        cfg.daemon.coalesce,
        now,
    );
    for key in due {
        let mut core = state.core();
        let Some(entry) = core.cache.get_mut(&key) else {
            continue;
        };
        if entry.in_flight {
            continue;
        }
        entry.in_flight = true;
        let identity = Arc::clone(&entry.identity);
        drop(core);
        spawn_fetch(Arc::clone(state), key, identity, Arc::clone(&cfg), None);
    }
    cache::evict(
        &mut state.core().cache,
        cfg.daemon.path_evict,
        cfg.daemon.max_paths,
        now,
    );
}

/// Clears a key's single-flight flag if its fetch task ends without writing back (a panic in
/// a provider, or the runtime dropping the task), so the key can't stay in flight forever.
/// Checked like the write-back: a task for an evicted entry must not release its
/// replacement's flag.
struct InFlightGuard {
    state: Arc<Daemon>,
    key: cache::Key,
    identity: Arc<()>,
    armed: bool,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // No panic in Drop: it may run during unwinding, with the lock poisoned.
        let mut core = self
            .state
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = core.cache.get_mut(&self.key)
            && Arc::ptr_eq(&entry.identity, &self.identity)
        {
            entry.in_flight = false;
        }
    }
}

/// Runs a fetch outside any lock, then writes the result back (single-flight release,
/// stale value kept on failure, backoff on failure, a short retry on pool saturation).
///
/// An entry has at most one fetch in flight, even across a reload (which keeps the flag), so
/// a matching identity always releases it. The result is kept only if the badge's config is
/// still the one it ran under: a reload that changed the badge already marked the entry stale,
/// and the loop refetches it with the new config.
fn spawn_fetch(
    state: Arc<Daemon>,
    key: cache::Key,
    identity: Arc<()>,
    cfg: Arc<Config>,
    done: Option<oneshot::Sender<()>>,
) {
    tokio::spawn(async move {
        let mut guard = InFlightGuard {
            state: Arc::clone(&state),
            key: key.clone(),
            identity: Arc::clone(&identity),
            armed: true,
        };
        let result = match cfg.badge.get(&key.badge) {
            Some(badge_cfg) => Some(
                provider::fetch(
                    &state.providers,
                    badge_cfg,
                    key.root.as_deref(),
                    key.env.as_deref(),
                )
                .await,
            ),
            None => None,
        };
        write_back(
            &mut state.core(),
            &key,
            &identity,
            &cfg,
            result,
            Instant::now(),
        );
        guard.armed = false;
        state.reschedule.notify_one();
        if let Some(tx) = done {
            let _ = tx.send(());
        }
    });
}

/// Stores a fetch's result in its entry, if the entry is still the one it ran for (see
/// [`spawn_fetch`]).
fn write_back(
    core: &mut Core,
    key: &cache::Key,
    identity: &Arc<()>,
    cfg: &Arc<Config>,
    result: Option<Result<String, provider::ProviderError>>,
    now: Instant,
) {
    let Core { config, cache } = core;
    let Some(entry) = cache
        .get_mut(key)
        .filter(|e| Arc::ptr_eq(&e.identity, identity))
    else {
        return;
    };
    entry.in_flight = false;
    let daemon = &config.daemon;
    let unchanged =
        Arc::ptr_eq(config, cfg) || config.badge.get(&key.badge) == cfg.badge.get(&key.badge);
    match result.filter(|_| unchanged) {
        Some(Ok(rendered)) => {
            entry.value = Some(rendered);
            entry.fetched_at = Some(now);
            entry.errors = 0;
            entry.next_attempt = now;
        }
        // The fetch never ran: not the badge's failure. Retry after `retry_min` (not at once:
        // the entry would stay due and spin the scheduler) without growing the backoff.
        Some(Err(provider::ProviderError::Saturated)) => {
            entry.next_attempt = now + daemon.retry_min;
        }
        Some(Err(_)) => {
            entry.errors += 1;
            entry.next_attempt =
                now + cache::backoff(daemon.retry_min, daemon.retry_max, entry.errors);
        }
        None => {}
    }
}

/// Runs the path-scoped idle-eviction pass at most once every `config_check`, the same
/// throttle `check_reload` uses. The `max_paths` LRU cap itself is enforced immediately on
/// insert (see `resolve::plan_badge`); this only needs to catch keys going idle, which is
/// cheap to delay.
fn maybe_evict_idle(state: &Daemon, cfg: &Config, now: Instant) {
    if state
        .last_evict
        .claim_if_elapsed(now, cfg.daemon.config_check)
    {
        cache::evict_idle(&mut state.core().cache, cfg.daemon.path_evict, now);
    }
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

    pub(super) fn test_daemon(power: cache::Power) -> Arc<Daemon> {
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
            PowerState {
                value: power,
                checked_at: now,
            },
            upgrade::Upgrade::new(None, now),
            now,
        ))
    }

    impl Daemon {
        pub(super) fn set_config(&self, cfg: &Arc<Config>) {
            self.core().config = Arc::clone(cfg);
        }

        pub(super) fn insert(&self, key: &cache::Key, entry: cache::Entry) -> Arc<()> {
            let identity = Arc::clone(&entry.identity);
            self.core().cache.insert(key.clone(), entry);
            identity
        }

        pub(super) fn entry<T>(&self, key: &cache::Key, f: impl FnOnce(&cache::Entry) -> T) -> T {
            f(&self.core().cache[key])
        }
    }

    async fn run_fetch(state: &Arc<Daemon>, key: &cache::Key, identity: Arc<()>, cfg: Arc<Config>) {
        let (tx, rx) = oneshot::channel();
        spawn_fetch(Arc::clone(state), key.clone(), identity, cfg, Some(tx));
        tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();
    }

    fn in_flight_entry(value: Option<&str>) -> cache::Entry {
        let mut entry = cache::Entry::new(Instant::now());
        entry.value = value.map(str::to_string);
        entry.in_flight = true;
        entry
    }

    #[tokio::test]
    async fn fetch_for_a_changed_badge_releases_the_flag_but_keeps_the_value() {
        let state = test_daemon(cache::Power::Ac);
        // Same badge, edited interval.
        state.set_config(&Arc::new(test_cfg(
            Duration::from_secs(61),
            Duration::from_secs(60),
        )));
        let old_cfg = Arc::new(test_cfg(Duration::from_secs(60), Duration::from_secs(60)));
        let key = cache::Key::global("b");
        let identity = state.insert(&key, in_flight_entry(Some("kept")));
        run_fetch(&state, &key, identity, old_cfg).await;
        let (value, in_flight) = state.entry(&key, |e| (e.value.clone(), e.in_flight));
        assert_eq!(
            value.as_deref(),
            Some("kept"),
            "old-config result discarded"
        );
        assert!(!in_flight, "the entry's only fetch has finished");
    }

    #[tokio::test]
    async fn fetch_across_a_reload_that_kept_the_badge_writes_back() {
        let state = test_daemon(cache::Power::Ac);
        state.set_config(&Arc::new(test_cfg(
            Duration::from_secs(60),
            Duration::from_secs(60),
        )));
        // A different `Config` (as after a reload) with an equal badge `b`.
        let old_cfg = Arc::new(test_cfg(Duration::from_secs(60), Duration::from_secs(60)));
        let key = cache::Key::global("b");
        let identity = state.insert(&key, in_flight_entry(None));
        run_fetch(&state, &key, identity, old_cfg).await;
        let (value, in_flight) = state.entry(&key, |e| (e.value.clone(), e.in_flight));
        assert!(value.is_some(), "unchanged badge: the result is current");
        assert!(!in_flight);
    }

    #[tokio::test]
    async fn saturated_pool_retries_without_counting_an_error() {
        let state = test_daemon(cache::Power::Ac);
        let cfg = Arc::new(test_cfg(Duration::from_secs(60), Duration::from_secs(60)));
        state.set_config(&cfg);
        let key = cache::Key::global("b");
        let mut entry = in_flight_entry(None);
        entry.errors = 2;
        let identity = state.insert(&key, entry);
        let held = state.providers.blocking.hold_all();
        let before = Instant::now();
        run_fetch(&state, &key, identity, cfg).await;
        drop(held);
        let (in_flight, errors, next_attempt) =
            state.entry(&key, |e| (e.in_flight, e.errors, e.next_attempt));
        assert!(!in_flight);
        assert_eq!(errors, 2, "saturation is not the badge's failure");
        assert!(next_attempt >= before + DaemonConfig::default().retry_min);
    }

    #[tokio::test]
    async fn in_flight_guard_clears_the_flag_when_a_fetch_task_panics() {
        let state = test_daemon(cache::Power::Ac);
        let key = cache::Key::global("b");
        let identity = state.insert(&key, in_flight_entry(None));
        let guard = |identity: &Arc<()>| InFlightGuard {
            state: Arc::clone(&state),
            key: key.clone(),
            identity: Arc::clone(identity),
            armed: true,
        };

        // A guard for an evicted entry must not release its replacement's flag.
        drop(guard(&Arc::new(())));
        assert!(state.entry(&key, |e| e.in_flight));

        let g = guard(&identity);
        let task = tokio::spawn(async move {
            let _g = g;
            panic!("provider bug");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert!(!state.entry(&key, |e| e.in_flight));
    }

    #[tokio::test]
    async fn evicted_fetch_cannot_overwrite_a_replacement_entry() {
        let state = test_daemon(cache::Power::Ac);
        let cfg = Arc::new(test_cfg(Duration::from_secs(60), Duration::from_secs(60)));
        state.set_config(&cfg);
        let key = cache::Key::global("b");
        let old_identity = cache::Entry::new(Instant::now()).identity;
        state.insert(&key, in_flight_entry(Some("replacement-value")));
        run_fetch(&state, &key, old_identity, cfg).await;
        let (value, in_flight) = state.entry(&key, |e| (e.value.clone(), e.in_flight));
        assert_eq!(value.as_deref(), Some("replacement-value"));
        assert!(in_flight, "must not release a replacement fetch");
    }

    #[test]
    fn atomic_instant_claims_once_per_gap() {
        let t0 = Instant::now();
        let gap = Duration::from_secs(10);
        let at = AtomicInstant::new(t0);
        assert!(!at.claim_if_elapsed(t0 + Duration::from_secs(9), gap));
        assert!(at.claim_if_elapsed(t0 + gap, gap));
        assert_eq!(at.get(), t0 + gap);
        assert!(!at.claim_if_elapsed(t0 + gap, gap), "the window is taken");
        // Before the epoch saturates to it rather than wrapping.
        at.set(t0.checked_sub(Duration::from_secs(1)).unwrap_or(t0));
        assert_eq!(at.get(), t0);
    }
}
