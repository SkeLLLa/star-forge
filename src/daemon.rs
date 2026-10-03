//! setsid/chdir, flock singleton, accept loop, request handling, reload/stop, idle exit.
//!
//! Scheduling is one loop: `select! { accept, sleep_until(next_due), signals }`. No
//! per-badge tasks, no fixed tick; refreshes are demand-driven and coalesced (see
//! `cache.rs`).
//!
//! The overall idea — one daemon that computes statusline values once and serves them from
//! a cache to every prompt/tmux caller over a Unix socket — comes from
//! [beachcomber](https://github.com/NavistAu/beachcomber); see `THIRD_PARTY_NOTICES.md`.

use std::collections::{BTreeMap, HashMap};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::io::AsyncWriteExt as _;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Notify, oneshot};

use crate::config::{Builtin, Config, Scope, Source};
use crate::{cache, extract, ipc, provider};

/// `(dev, ino, len, ctime, ctime_nsec)` of a file, following symlinks.
type FileId = (u64, u64, u64, i64, i64);
type Stamp = (Option<FileId>, Option<FileId>);

/// Not mtime alone: `cp -p`/`rsync -a` can preserve it, and Nix store files all share mtime 1,
/// so re-pointing a symlink (home-manager) would go unnoticed. ctime can't be set from
/// userspace, and the inode changes on rename-on-save and on a symlink retarget.
fn file_id(p: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok()?;
    Some((m.dev(), m.ino(), m.len(), m.ctime(), m.ctime_nsec()))
}

fn stamp(config: &Path, palette: Option<&Path>) -> Stamp {
    (file_id(config), palette.and_then(file_id))
}

struct ConfigMeta {
    path: PathBuf,
    /// Config file + `palette_from` file identities; a change in either triggers a reload.
    stamp: Stamp,
    last_checked: Instant,
    checking: bool,
}

struct OnAc {
    value: bool,
    checked_at: Instant,
}

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
    // leader, which the client's spawn deliberately avoids (see `client::try_spawn_daemon`).
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
                let st = cfg.as_ref().map_or((None, None), |c| {
                    (
                        config_id,
                        c.daemon.palette_from.as_deref().and_then(file_id),
                    )
                });
                (cfg, st)
            },
        )
        .await;
    let (loaded, stamp) = loaded.unwrap_or_else(|| {
        (
            Err("config read timed out or filesystem workers are busy".into()),
            (None, None),
        )
    });
    let initial = loaded.unwrap_or_else(|e| {
        eprintln!("stfgd: config load failed, starting with an empty config: {e}");
        Config {
            daemon: crate::config::DaemonConfig::default(),
            badge: BTreeMap::new(),
            groups: BTreeMap::new(),
        }
    });
    let now = Instant::now();
    let power_checked_at = now.checked_sub(initial.daemon.power_check).unwrap_or(now);

    let state = Arc::new(Daemon {
        cache: Mutex::new(HashMap::new()),
        config: Mutex::new(Arc::new(initial)),
        config_meta: Mutex::new(ConfigMeta {
            path: config_path,
            stamp,
            last_checked: now,
            checking: false,
        }),
        providers,
        last_request: Mutex::new(now),
        last_evict: Mutex::new(now),
        on_ac: Mutex::new(OnAc {
            value: true,
            checked_at: power_checked_at,
        }),
        shutdown: Notify::new(),
        reload_finished: Notify::new(),
        reschedule: Notify::new(),
    });

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
        let idle_exit = state.config.lock().unwrap().daemon.idle_exit;
        let idle_deadline = *state.last_request.lock().unwrap() + idle_exit;

        let on_ac = get_on_ac(&state, now);
        let next_due = {
            let cache = state.cache.lock().unwrap();
            let cfg = state.config.lock().unwrap();
            cache::next_due(&cache, &cfg.badge, on_ac, now)
        };
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

fn get_on_ac(state: &Arc<Daemon>, now: Instant) -> bool {
    let mut g = state.on_ac.lock().unwrap();
    let power_check = state.config.lock().unwrap().daemon.power_check;
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
    let cfg = Arc::clone(&state.config.lock().unwrap());
    let on_ac = get_on_ac(state, now);
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
        let cwd = key.1.clone();
        let result = match cfg.badge.get(&key.0) {
            Some(badge_cfg) => {
                Some(provider::fetch(&state.providers, badge_cfg, cwd.as_deref()).await)
            }
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

/// Stats the config file at most once every `config_check` (bypassed by an explicit
/// `reload` command) and reloads if it, or its `palette_from` file, changed (inode, size or ctime). A config that fails to parse or
/// validate is logged and ignored; the previous config keeps running.
async fn check_reload(state: &Arc<Daemon>, force: bool, now: Instant) {
    let (config_check, palette) = {
        let cfg = state.config.lock().unwrap();
        (cfg.daemon.config_check, cfg.daemon.palette_from.clone())
    };
    let deadline = tokio::time::Instant::now() + MAINTENANCE_BUDGET;
    let (path, previous) = loop {
        let notified = state.reload_finished.notified();
        let ready = {
            let mut meta = state.config_meta.lock().unwrap();
            if !force && now.saturating_duration_since(meta.last_checked) < config_check {
                return;
            }
            if meta.checking {
                None
            } else {
                meta.checking = true;
                meta.last_checked = now;
                Some((meta.path.clone(), meta.stamp))
            }
        };
        if let Some(ready) = ready {
            break ready;
        }
        if !force || tokio::time::timeout_at(deadline, notified).await.is_err() {
            return;
        }
    };
    let result = state
        .providers
        .blocking
        .run(deadline, move || {
            let pre = stamp(&path, palette.as_deref());
            if !force && previous == pre {
                return None;
            }
            let cfg = Config::load(&path);
            // Stamped before the load so an edit during it is seen next check. ponytail: if
            // `palette_from` changed, the new file is stat'ed after the load (tiny window).
            let st = match &cfg {
                Ok(c) if c.daemon.palette_from.as_deref() != palette.as_deref() => {
                    (pre.0, c.daemon.palette_from.as_deref().and_then(file_id))
                }
                _ => pre,
            };
            Some((st, cfg))
        })
        .await;
    match result {
        Some(Some((st, Ok(new_cfg)))) => {
            state.config_meta.lock().unwrap().stamp = st;
            let mut current = state.config.lock().unwrap();
            let mut cache = state.cache.lock().unwrap();
            cache.retain(|(name, root), entry| {
                let Some(badge) = new_cfg.badge.get(name) else {
                    return false;
                };
                if root.is_some() != (badge.scope == Scope::GitRoot) {
                    return false;
                }
                // Keep activity and the last good value, but refresh under the new
                // config. Old fetch completions are rejected by Arc identity above.
                entry.fetched_at = None;
                entry.in_flight = false;
                entry.fingerprint = None;
                entry.errors = 0;
                entry.next_attempt = Instant::now();
                true
            });
            drop(cache);
            *current = Arc::new(new_cfg);
            drop(current);
            state.reschedule.notify_one();
        }
        Some(Some((_, Err(e)))) => {
            eprintln!("stfgd: config reload failed, keeping previous config: {e}");
        }
        None => {
            eprintln!("stfgd: config check timed out or filesystem workers are busy");
        }
        Some(None) => {}
    }
    state.config_meta.lock().unwrap().checking = false;
    state.reload_finished.notify_waiters();
}

/// Implicit checks must not consume the prompt's latency budget.
fn schedule_reload_check(state: &Arc<Daemon>, now: Instant) {
    let state = Arc::clone(state);
    tokio::spawn(async move { check_reload(&state, false, now).await });
}

/// Reads one request line and dispatches on its first byte: `{` is JSON (admin commands),
/// `g` is the plain-text `get` protocol (see `client.rs`). Anything else (including an
/// empty line) drops the connection.
async fn handle_connection(state: Arc<Daemon>, mut stream: UnixStream) {
    let now = Instant::now();
    *state.last_request.lock().unwrap() = now;

    let Ok(Ok(line)) = tokio::time::timeout(
        Duration::from_secs(2),
        ipc::read_line_bytes_capped_async(&mut stream, ipc::MAX_LINE_BYTES),
    )
    .await
    else {
        return;
    };

    match line.first() {
        Some(b'{') => handle_json(&state, &mut stream, &line, now).await,
        Some(b'g') => handle_text_get(&state, &mut stream, &line, now).await,
        _ => {}
    }
}

/// JSON admin commands: `status`/`stop`/`reload`. `get` no longer goes through JSON (see
/// `handle_text_get`).
async fn handle_json(state: &Arc<Daemon>, stream: &mut UnixStream, line: &[u8], now: Instant) {
    let Ok(text) = std::str::from_utf8(line) else {
        return;
    };
    let Ok(req) = serde_json::from_str::<ipc::Request>(text) else {
        return;
    };

    if req.cmd == "reload" {
        check_reload(state, true, now).await;
    } else {
        schedule_reload_check(state, now);
    }

    let resp = match req.cmd.as_str() {
        "status" => ipc::Response::ok(build_status(state)),
        "reload" | "stop" => {
            if req.cmd == "stop" {
                state.shutdown.notify_one();
            }
            ipc::Response::ok(BTreeMap::new())
        }
        _ => ipc::Response {
            v: ipc::PROTOCOL_VERSION,
            ver: env!("CARGO_PKG_VERSION").to_string(),
            ok: false,
            values: BTreeMap::new(),
        },
    };

    if let Ok(mut line) = serde_json::to_string(&resp) {
        line.push('\n');
        let _ =
            tokio::time::timeout(Duration::from_secs(2), stream.write_all(line.as_bytes())).await;
    }
}

/// Plain-text `get` (see `client.rs`'s wire protocol doc): one line, fields separated by
/// 0x1F: `get<0x1F><ver><0x1F><cwd>(<0x1F><badge>)*`. Response is one line per requested
/// badge, in order, then EOF (the connection closes when this function returns).
async fn handle_text_get(state: &Arc<Daemon>, stream: &mut UnixStream, line: &[u8], now: Instant) {
    let Ok(text) = std::str::from_utf8(line) else {
        return;
    };
    let mut fields = text.split('\u{1f}');
    let _cmd = fields.next(); // "get"
    let ver = fields.next().unwrap_or_default();
    let cwd = fields.next().unwrap_or_default();
    let badges: Vec<String> = fields.map(str::to_string).collect();

    schedule_reload_check(state, now);
    let cfg = Arc::clone(&state.config.lock().unwrap());
    let cwd_opt = (!cwd.is_empty()).then_some(cwd);
    let values = handle_get(state, &cfg, cwd_opt, &fetch_names(&cfg, &badges)).await;
    let out = frame_lines(response_lines(&cfg, &badges, &values));
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.write_all(out.as_bytes())).await;

    // Upgrade safety (design.md §1): a `get` from a different client version gets a normal
    // answer first, then this daemon shuts itself down (killpg in-flight, unlink socket,
    // exit) via the same graceful path as `stop`. Zero extra client latency: no
    // synchronous notification round-trip on the hot path.
    if !ver.is_empty() && ver != env!("CARGO_PKG_VERSION") {
        state.shutdown.notify_one();
    }
}

/// Groups resolve daemon-side: members are fetched exactly as if requested individually,
/// then joined into the group's single line by `response_lines`.
fn fetch_names(cfg: &Config, requested: &[String]) -> Vec<String> {
    requested
        .iter()
        .flat_map(|n| {
            cfg.groups
                .get(n)
                .map_or_else(|| std::slice::from_ref(n), |g| &g.badges[..])
        })
        .cloned()
        .collect()
}

/// One `\n`-terminated line per value; embedded `\n`/`\r` would break line framing, so
/// they're replaced with a space.
fn frame_lines(lines: Vec<String>) -> String {
    let mut out = String::new();
    for value in lines {
        out.extend(
            value
                .chars()
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c }),
        );
        out.push('\n');
    }
    out
}

/// One output line per requested name: a badge's own value (empty if unknown), or for a
/// group its members' non-empty values joined by the group separator, in member order.
fn response_lines(
    cfg: &Config,
    requested: &[String],
    values: &BTreeMap<String, String>,
) -> Vec<String> {
    let get = |n: &String| values.get(n).map_or("", String::as_str);
    requested
        .iter()
        .map(|name| {
            cfg.groups.get(name).map_or_else(
                || get(name).to_string(),
                |g| {
                    g.template.as_ref().map_or_else(
                        || {
                            let parts: Vec<_> = g
                                .badges
                                .iter()
                                .map(get)
                                .filter(|v| !v.is_empty())
                                .map(|v| g.output.escape(v))
                                .collect();
                            parts.join(&*g.output.escape(&g.separator))
                        },
                        |t| t.render(g.output, |n| get(&n.to_string()).to_string()),
                    )
                },
            )
        })
        .collect()
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

/// `true` for the two git builtins still backed by a subprocess (`git_status`/
/// `git_counts`): their cache entries get request-time fingerprint invalidation (see
/// `handle_get`) instead of waiting out `interval`, because unlike the other git builtins
/// they're too expensive to just recompute in-process every request.
const fn needs_fingerprint(source: &Source) -> bool {
    matches!(
        source,
        Source::Builtin {
            name: Builtin::GitStatus | Builtin::GitCounts,
            ..
        }
    )
}

/// Renders a raw builtin value through the badge's normal extract/format pipeline, without
/// going through `provider::fetch`'s cache/subprocess machinery. Used for the in-process git
/// builtins (see `provider::run_in_process_git`); an extract failure renders empty, same as
/// an empty value would.
fn render_raw(badge_cfg: &crate::config::BadgeConfig, raw: &[u8]) -> String {
    extract::apply(badge_cfg.extract.as_ref(), raw).map_or_else(
        |_| String::new(),
        |value| extract::render(&badge_cfg.format, &value),
    )
}

#[derive(Default)]
struct GitSnapshot {
    root: Option<PathBuf>,
    fingerprint: Option<provider::GitFingerprint>,
    immediate: BTreeMap<String, String>,
}

/// One bounded job performs all request-time git reads. This also avoids repeatedly
/// statting HEAD/index when several git-count badges are requested together.
fn git_snapshot(cfg: &Config, cwd: &Path, badges: &[String]) -> GitSnapshot {
    let root = provider::git_root(cwd);
    // Resolved once per request, shared by every in-process git badge and the fingerprint.
    let dir = root.as_deref().and_then(provider::git_dir);
    let mut immediate = BTreeMap::new();
    let mut fingerprinted = false;
    for name in badges {
        let Some(badge) = cfg.badge.get(name) else {
            continue;
        };
        fingerprinted |= needs_fingerprint(&badge.source);
        if let Source::Builtin { name: builtin, .. } = badge.source
            && let Some(raw) = provider::run_in_process_git(builtin, dir.as_deref())
        {
            immediate.insert(name.clone(), render_raw(badge, &raw));
        }
    }
    let fingerprint = if fingerprinted {
        root.as_ref().map(|_| {
            dir.as_deref()
                .map(provider::git_fingerprint)
                .unwrap_or_default()
        })
    } else {
        None
    };
    GitSnapshot {
        root,
        fingerprint,
        immediate,
    }
}

/// Answers a `get`: cache hit returns the last good value immediately (stale-while-
/// revalidate); a stale/missing value triggers a background refresh; a truly cold key (no
/// value at all) waits up to the configured `cold_wait` for the fetch it just started.
async fn handle_get(
    state: &Arc<Daemon>,
    cfg: &Arc<Config>,
    cwd: Option<&str>,
    badges: &[String],
) -> BTreeMap<String, String> {
    let now = Instant::now();
    // Filesystem reads and cold fetch waits share one budget, rather than adding a
    // timeout per badge. Even cold_wait=0 permits bounded fresh in-process git reads.
    let deadline =
        tokio::time::Instant::now() + cfg.daemon.cold_wait.max(Duration::from_millis(20));
    let needs_repo = badges.iter().any(|name| {
        cfg.badge
            .get(name)
            .is_some_and(|b| b.scope == Scope::GitRoot)
    });
    let snapshot = if needs_repo && let Some(cwd) = cwd {
        let cwd = PathBuf::from(cwd);
        let cfg = Arc::clone(cfg);
        let badges = badges.to_vec();
        state
            .providers
            .blocking
            .run(deadline, move || git_snapshot(&cfg, &cwd, &badges))
            .await
            .unwrap_or_default()
    } else {
        GitSnapshot::default()
    };
    // Reload may have completed while the filesystem job was running. Do not create
    // old-config cache entries after that reload's invalidation pass.
    if !Arc::ptr_eq(&state.config.lock().unwrap(), cfg) {
        return badges
            .iter()
            .map(|name| (name.clone(), String::new()))
            .collect();
    }
    let repo_root = snapshot.root;
    let on_ac = get_on_ac(state, now);
    maybe_evict_idle(state, cfg, now);

    let mut values = BTreeMap::new();
    let mut cold_waiters: Vec<(String, cache::Key, oneshot::Receiver<()>)> = Vec::new();

    for name in badges {
        let Some(badge_cfg) = cfg.badge.get(name) else {
            values.insert(name.clone(), String::new());
            continue;
        };

        // Fresh in-process git values were computed on a worker, never on the reactor.
        if let Some(value) = snapshot.immediate.get(name) {
            values.insert(name.clone(), value.clone());
            continue;
        }

        let is_git_root = badge_cfg.scope == Scope::GitRoot;
        if is_git_root && repo_root.is_none() {
            // Not a git repo: render empty, no fetch at all.
            values.insert(name.clone(), String::new());
            continue;
        }
        let scope = if is_git_root { repo_root.clone() } else { None };
        let key: cache::Key = (name.clone(), scope);
        let current_fp = needs_fingerprint(&badge_cfg.source)
            .then_some(snapshot.fingerprint)
            .flatten();

        let mut cache = state.cache.lock().unwrap();
        let is_new_path_scoped = is_git_root && !cache.contains_key(&key);
        let entry = cache
            .entry(key.clone())
            .or_insert_with(|| cache::Entry::new(now));
        entry.last_access = now;
        let interval = cache::effective_interval(badge_cfg, on_ac);
        // Fingerprint changed since the fetch that produced the current value started:
        // force both stale and cold so the refreshing fetch lands in this same prompt
        // instead of waiting out `interval` (item 2 — unstaged worktree edits, which touch
        // neither `.git/HEAD` nor `.git/index`, still only surface on `interval`).
        let fingerprint_changed =
            current_fp.is_some_and(|fp| entry.fingerprint.is_some_and(|prev| prev != fp));
        let stale = fingerprint_changed
            || entry
                .fetched_at
                .is_none_or(|t| now.duration_since(t) >= interval);
        let cold_miss = fingerprint_changed || entry.value.is_none();
        let should_spawn = stale && !entry.in_flight && now >= entry.next_attempt;
        if should_spawn {
            entry.in_flight = true;
            if let Some(fp) = current_fp {
                entry.fingerprint = Some(fp);
            }
        }
        let identity = Arc::clone(&entry.identity);
        let current_value = entry.value.clone();
        // Enforce the path-scoped cap (LRU by last_access) right away: only the timer
        // path ran this before, so a config with only path-scoped (e.g. git) badges
        // could exceed it for up to `idle_exit`.
        if is_new_path_scoped {
            cache::enforce_path_scoped_cap(&mut cache, cfg.daemon.max_paths);
        }
        drop(cache);

        if should_spawn {
            let done = if cold_miss {
                let (tx, rx) = oneshot::channel();
                cold_waiters.push((name.clone(), key.clone(), rx));
                Some(tx)
            } else {
                None
            };
            spawn_fetch(Arc::clone(state), key, identity, Arc::clone(cfg), done);
        }

        values.insert(name.clone(), current_value.unwrap_or_default());
    }

    if !cold_waiters.is_empty() {
        let keys: Vec<(String, cache::Key)> = cold_waiters
            .iter()
            .map(|(n, k, _)| (n.clone(), k.clone()))
            .collect();
        let wait_all = async {
            for (_, _, rx) in cold_waiters {
                let _ = rx.await;
            }
        };
        let wait_deadline = deadline.min(tokio::time::Instant::now() + cfg.daemon.cold_wait);
        let _ = tokio::time::timeout_at(wait_deadline, wait_all).await;
        let cache = state.cache.lock().unwrap();
        for (name, key) in keys {
            if let Some(entry) = cache.get(&key) {
                values.insert(name, entry.value.clone().unwrap_or_default());
            }
        }
    }

    values
}

fn build_status(state: &Daemon) -> BTreeMap<String, String> {
    let now = Instant::now();
    let mut rows: Vec<(cache::Key, Option<Instant>, u32)> = {
        let cache = state.cache.lock().unwrap();
        cache
            .iter()
            .map(|(k, e)| (k.clone(), e.fetched_at, e.errors))
            .collect()
    };
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut lines = vec![format!(
        "{:<20} {:<40} {:>8} {:>7}",
        "badge", "scope", "age_s", "errors"
    )];
    for (key, fetched_at, errors) in rows {
        let scope = key
            .1
            .as_ref()
            .map_or_else(|| "-".to_string(), |p| p.display().to_string());
        let age = fetched_at.map_or_else(
            || "-".to_string(),
            |t| now.saturating_duration_since(t).as_secs().to_string(),
        );
        lines.push(format!(
            "{:<20} {:<40} {:>8} {:>7}",
            key.0, scope, age, errors
        ));
    }

    let mut values = BTreeMap::new();
    values.insert("status".to_string(), lines.join("\n"));
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BadgeConfig, DaemonConfig};
    use crate::template::Dialect;

    fn test_cfg(interval: Duration, battery_interval: Duration) -> Config {
        let mut badge = BTreeMap::new();
        badge.insert(
            "b".to_string(),
            BadgeConfig {
                source: Source::Command {
                    command: "true".into(),
                    args: None,
                },
                extract: None,
                format: "{value}".into(),
                interval,
                battery_interval,
                active_window: Duration::from_secs(300),
                timeout: Duration::from_secs(2),
                max_output: 1024,
                scope: Scope::Global,
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

    #[test]
    fn group_lines_skip_empty_keep_order() {
        let mut cfg = test_cfg(Duration::from_secs(60), Duration::from_secs(60));
        cfg.groups.insert(
            "g".into(),
            crate::config::Group {
                badges: vec!["c".into(), "b".into(), "a".into()],
                separator: "|".into(),
                template: None,
                output: Dialect::Ansi,
            },
        );
        let v = |k: &str, x: &str| (k.to_string(), x.to_string());
        let values = BTreeMap::from([v("a", "1"), v("b", ""), v("c", "3")]);
        let req = ["g", "a", "nope"].map(String::from);
        assert_eq!(response_lines(&cfg, &req, &values), ["3|1", "1", ""]);
    }

    #[test]
    fn tmux_plain_group_escapes_values_and_separator() {
        let mut cfg = test_cfg(Duration::from_secs(60), Duration::from_secs(60));
        cfg.groups.insert(
            "g".into(),
            crate::config::Group {
                badges: vec!["a".into(), "b".into()],
                separator: " # ".into(),
                template: None,
                output: Dialect::Tmux,
            },
        );
        let v = |k: &str, x: &str| (k.to_string(), x.to_string());
        let values = BTreeMap::from([v("a", "a#b"), v("b", "#{x}")]);
        let req = ["g"].map(String::from);
        assert_eq!(response_lines(&cfg, &req, &values), ["a##b ## ##{x}"]);
    }

    #[test]
    fn fetch_names_expands_groups_in_order() {
        let mut cfg = test_cfg(Duration::from_secs(60), Duration::from_secs(60));
        cfg.groups.insert(
            "g".into(),
            crate::config::Group {
                badges: vec!["a".into(), "b".into()],
                separator: " ".into(),
                template: None,
                output: Dialect::Ansi,
            },
        );
        let req = ["x", "g", "a", "nope"].map(String::from);
        assert_eq!(fetch_names(&cfg, &req), ["x", "a", "b", "a", "nope"]);
    }

    #[test]
    fn frame_lines_flattens_newlines() {
        let lines = vec!["a\nb".into(), String::new(), "c\r\nd".into()];
        assert_eq!(frame_lines(lines), "a b\n\nc  d\n");
    }

    #[test]
    fn stamp_tracks_both_files() {
        let dir = std::env::temp_dir().join(format!("stfg-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (c, p) = (dir.join("c.toml"), dir.join("p.toml"));
        assert_eq!(stamp(&c, Some(&p)), (None, None));
        std::fs::write(&c, "").unwrap();
        let s1 = stamp(&c, Some(&p));
        assert!(s1.0.is_some() && s1.1.is_none());
        std::fs::write(&p, "").unwrap();
        let s2 = stamp(&c, Some(&p));
        assert_ne!(s1, s2);
        assert_eq!(stamp(&c, None).1, None);
        // Same mtime, different content/inode (rsync -a, Nix symlink retarget) is still seen.
        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        let p2 = dir.join("p2.toml");
        std::fs::write(&p2, "x").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&p2)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        std::fs::rename(&p2, &p).unwrap();
        assert_ne!(stamp(&c, Some(&p)), s2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn test_daemon(on_ac: bool) -> Arc<Daemon> {
        let now = Instant::now();
        Arc::new(Daemon {
            cache: Mutex::new(HashMap::new()),
            config: Mutex::new(Arc::new(Config {
                daemon: DaemonConfig::default(),
                badge: BTreeMap::new(),
                groups: BTreeMap::new(),
            })),
            config_meta: Mutex::new(ConfigMeta {
                path: PathBuf::new(),
                stamp: (None, None),
                last_checked: now,
                checking: false,
            }),
            providers: provider::Shared::new(),
            last_request: Mutex::new(now),
            last_evict: Mutex::new(now),
            on_ac: Mutex::new(OnAc {
                value: on_ac,
                checked_at: now,
            }),
            shutdown: Notify::new(),
            reload_finished: Notify::new(),
            reschedule: Notify::new(),
        })
    }

    #[test]
    fn response_lines_template_group_with_plain_and_unknown() {
        let mut cfg = test_cfg(Duration::from_secs(60), Duration::from_secs(60));
        let t = crate::template::Template::parse("[{a}](red)(-{b})", &BTreeMap::new()).unwrap();
        cfg.groups.insert(
            "g".into(),
            crate::config::Group {
                badges: t.vars().to_vec(),
                separator: String::new(),
                template: Some(t),
                output: Dialect::Tmux,
            },
        );
        let v = |k: &str, x: &str| (k.to_string(), x.to_string());
        let values = BTreeMap::from([v("a", "1#"), v("b", "")]);
        let req = ["g", "a", "a", "g"].map(String::from);
        assert_eq!(
            response_lines(&cfg, &req, &values),
            [
                "#[fg=red]1###[default]",
                "1#",
                "1#",
                "#[fg=red]1###[default]"
            ]
        );
    }

    #[tokio::test]
    async fn handle_get_duplicate_names_fetch_once() {
        let cfg = Arc::new(test_cfg(Duration::from_secs(10), Duration::from_secs(10)));
        let state = test_daemon(true);
        *state.config.lock().unwrap() = Arc::clone(&cfg);
        let req = ["b", "b", "nope"].map(String::from);
        let values = handle_get(&state, &cfg, None, &req).await;
        assert_eq!(values.len(), 2);
        assert_eq!(values["nope"], "");
        assert_eq!(state.cache.lock().unwrap().len(), 1);
    }

    /// Temp dir with a config (`c.toml`, group `r` = `[{a}](brand)`) and palette (`p.toml`).
    struct ReloadEnv {
        dir: PathBuf,
        state: Arc<Daemon>,
    }

    impl ReloadEnv {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("stfg-reload-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let env = Self {
                dir,
                state: test_daemon(true),
            };
            env.write_palette("p.toml", "#112233");
            env.write_config("p.toml");
            let path = env.dir.join("c.toml");
            let cfg = Config::load(&path).unwrap();
            let st = stamp(&path, cfg.daemon.palette_from.as_deref());
            *env.state.config.lock().unwrap() = Arc::new(cfg);
            let mut meta = env.state.config_meta.lock().unwrap();
            meta.path = path;
            meta.stamp = st;
            drop(meta);
            env
        }

        fn write_config(&self, palette_file: &str) {
            std::fs::write(
                self.dir.join("c.toml"),
                format!(
                    "[daemon]\npalette_from = '{}'\n[badge.a]\ntype = \"builtin\"\nname = \"hostname\"\n\
                     [groups.r]\nformat = '[{{a}}](brand)'\n",
                    self.dir.join(palette_file).display()
                ),
            )
            .unwrap();
        }

        fn write_palette(&self, file: &str, color: &str) {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            std::fs::write(
                self.dir.join(file),
                // Unique size per write: ctime granularity can be coarser than the test.
                format!(
                    "palette = \"p\"\n[palettes.p]\nbrand = \"{color}\"\n#{}\n",
                    "x".repeat(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                ),
            )
            .unwrap();
        }

        fn cfg(&self) -> Arc<Config> {
            Arc::clone(&self.state.config.lock().unwrap())
        }

        fn rendered(&self) -> String {
            let cfg = self.cfg();
            let t = cfg.groups["r"].template.as_ref().unwrap();
            t.render(Dialect::Tmux, |_| "x".into())
        }

        /// Non-forced check, each one a "later" instant than the last (never throttled).
        async fn check(&self) {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let now = Instant::now() + Duration::from_secs(3600 * n);
            check_reload(&self.state, false, now).await;
        }
    }

    impl Drop for ReloadEnv {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn check_reload_unchanged_or_throttled_keeps_config() {
        let env = ReloadEnv::new("unchanged");
        let before = env.cfg();
        env.check().await;
        assert!(Arc::ptr_eq(&before, &env.cfg()));

        // Changed, but inside the `config_check` window: no stat, no reload.
        env.write_palette("p.toml", "#445566");
        let last = env.state.config_meta.lock().unwrap().last_checked;
        check_reload(&env.state, false, last).await;
        assert!(Arc::ptr_eq(&before, &env.cfg()));
        // Force bypasses the throttle (and the unchanged stamp).
        check_reload(&env.state, true, last).await;
        assert!(!Arc::ptr_eq(&before, &env.cfg()));
        assert_eq!(env.rendered(), "#[fg=#445566]x#[default]");
        assert!(!env.state.config_meta.lock().unwrap().checking);
    }

    #[tokio::test]
    async fn check_reload_palette_edit_reloads() {
        let env = ReloadEnv::new("edit");
        assert_eq!(env.rendered(), "#[fg=#112233]x#[default]");
        let before = env.cfg();
        env.write_palette("p.toml", "#abcdef");
        env.check().await;
        assert!(!Arc::ptr_eq(&before, &env.cfg()));
        assert_eq!(env.rendered(), "#[fg=#abcdef]x#[default]");
        // Stamp now matches disk: the next check is a no-op.
        let after = env.cfg();
        env.check().await;
        assert!(Arc::ptr_eq(&after, &env.cfg()));
    }

    #[tokio::test]
    async fn check_reload_broken_palette_keeps_old_and_retries() {
        let env = ReloadEnv::new("broken");
        let before = env.cfg();
        let old_stamp = env.state.config_meta.lock().unwrap().stamp;
        std::fs::write(env.dir.join("p.toml"), "not [[[ toml").unwrap();
        env.check().await;
        assert!(Arc::ptr_eq(&before, &env.cfg()));
        assert_eq!(env.state.config_meta.lock().unwrap().stamp, old_stamp);
        assert!(!env.state.config_meta.lock().unwrap().checking);
        // Still broken: retried (stamp not advanced), still old config.
        env.check().await;
        assert!(Arc::ptr_eq(&before, &env.cfg()));
        // Fixed: the retry succeeds.
        env.write_palette("p.toml", "#010203");
        env.check().await;
        assert!(!Arc::ptr_eq(&before, &env.cfg()));
        assert_eq!(env.rendered(), "#[fg=#010203]x#[default]");
    }

    #[tokio::test]
    async fn check_reload_palette_path_change_tracks_new_file() {
        let env = ReloadEnv::new("switch");
        env.write_palette("p2.toml", "#0000ff");
        env.write_config("p2.toml");
        env.check().await;
        assert_eq!(env.rendered(), "#[fg=#0000ff]x#[default]");
        let st = env.state.config_meta.lock().unwrap().stamp;
        assert_eq!(st.1, file_id(&env.dir.join("p2.toml")));
        assert!(st.1.is_some());
        // The new file is now the watched one; the old one is ignored.
        let cur = env.cfg();
        env.write_palette("p.toml", "#ff0000");
        env.check().await;
        assert!(Arc::ptr_eq(&cur, &env.cfg()));
        env.write_palette("p2.toml", "#00ff00");
        env.check().await;
        assert_eq!(env.rendered(), "#[fg=#00ff00]x#[default]");
    }

    /// Regression test for the bug item 3 fixes: `handle_get`'s staleness check must use
    /// the battery-aware `effective_interval` (same as the timer path), not always
    /// `badge_cfg.interval`.
    #[tokio::test]
    async fn handle_get_uses_battery_interval_when_on_battery() {
        let cfg = Arc::new(test_cfg(Duration::from_secs(10), Duration::from_millis(10)));
        let state = test_daemon(false); // on battery
        *state.config.lock().unwrap() = Arc::clone(&cfg);

        // Cold miss: spawns and cold-waits, so this returns only once the fetch lands.
        handle_get(&state, &cfg, None, &["b".to_string()]).await;
        let key: cache::Key = ("b".to_string(), None);
        assert!(!state.cache.lock().unwrap()[&key].in_flight);

        tokio::time::sleep(Duration::from_millis(50)).await; // > battery_interval, < interval

        // Warm this time: no cold wait, so `handle_get` returns before the runtime has a
        // chance to poll the background refresh it just spawned. `in_flight` still being
        // true proves `should_spawn` fired on `battery_interval` (10ms elapsed in 50ms),
        // not `interval` (10s) — the bug would leave this `false`.
        handle_get(&state, &cfg, None, &["b".to_string()]).await;
        assert!(state.cache.lock().unwrap()[&key].in_flight);
    }

    #[tokio::test]
    async fn old_config_fetch_cannot_overwrite_a_new_config_entry() {
        let state = test_daemon(true);
        let old_cfg = Arc::new(test_cfg(Duration::from_secs(60), Duration::from_secs(60)));
        let key: cache::Key = ("b".to_string(), None);
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
        let key: cache::Key = ("b".to_string(), None);
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
