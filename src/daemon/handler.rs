//! Request handling: connection dispatch, `get` resolution, status.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt as _;
use tokio::net::UnixStream;
use tokio::sync::oneshot;

use super::cache;
use super::reload::{check_reload, file_id, schedule_reload_check};
use super::{Daemon, maybe_evict_idle, refresh_on_ac_if_due, spawn_fetch};
use crate::config::{Config, Scope, Source};
use crate::{extract, ipc, provider};

/// Reads one request line and dispatches on its first byte: `{` is JSON (admin commands),
/// `g` is the plain-text `get` protocol (see `client.rs`). Anything else (including an
/// empty line) drops the connection.
pub(super) async fn handle_connection(state: Arc<Daemon>, mut stream: UnixStream) {
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
        "reload" => ipc::Response::ok(BTreeMap::new()), // already done above
        "stop" => {
            state.shutdown.notify_one();
            ipc::Response::ok(BTreeMap::new())
        }
        _ => ipc::Response::err(),
    };

    if let Ok(mut line) = serde_json::to_string(&resp) {
        line.push('\n');
        let _ =
            tokio::time::timeout(Duration::from_secs(2), stream.write_all(line.as_bytes())).await;
    }
}

/// The client's variables, borrowed from the request line.
type ClientEnv<'a> = Option<Vec<(&'a str, &'a str)>>;

/// Splits a `get` line into `(ver, cwd, badges, env)`. The optional env block follows the
/// badges, introduced by a `client::ENV_MARK` field; a request without it (an older client)
/// has an empty env, meaning the daemon's own. Values are never logged.
fn parse_get(text: &str) -> (&str, &str, Vec<String>, ClientEnv<'_>) {
    let mut fields = text.split('\u{1f}');
    let _cmd = fields.next(); // "get"
    let ver = fields.next().unwrap_or_default();
    let cwd = fields.next().unwrap_or_default();
    let mut badges = Vec::new();
    let mut marked = false;
    for f in fields.by_ref() {
        if f == crate::client::ENV_MARK {
            marked = true;
            break;
        }
        badges.push(f.to_string());
    }
    let env = marked.then(|| fields.filter_map(|f| f.split_once('=')).collect());
    (ver, cwd, badges, env)
}

/// The client's variables a badge's `env` allowlist selects, sorted: part of the cache key.
/// `None` when the badge has no `env` or the client sent none (the daemon's env is used).
fn select_env(patterns: &[String], env: Option<&[(&str, &str)]>) -> cache::EnvKey {
    let env = env.filter(|_| !patterns.is_empty())?;
    let mut out: Vec<_> = env
        .iter()
        .filter(|(k, _)| patterns.iter().any(|p| crate::config::env_matches(p, k)))
        .map(|&(k, v)| (k.to_string(), v.to_string()))
        .collect();
    out.sort();
    Some(out)
}

/// Plain-text `get` (see `client.rs`'s wire protocol doc): one line, fields separated by
/// 0x1F: `get<0x1F><ver><0x1F><cwd>(<0x1F><badge>)*(<0x1F><ENV_MARK>(<0x1F>NAME=value)*)?`.
/// Response is one line per requested badge, in order, then EOF (the connection closes when
/// this function returns).
async fn handle_text_get(state: &Arc<Daemon>, stream: &mut UnixStream, line: &[u8], now: Instant) {
    let Ok(text) = std::str::from_utf8(line) else {
        return;
    };
    let (ver, cwd, badges, env) = parse_get(text);

    schedule_reload_check(state, now);
    let cfg = state.config();
    let cwd_opt = (!cwd.is_empty()).then_some(cwd);
    let values = handle_get(
        state,
        &cfg,
        cwd_opt,
        env.as_deref(),
        &fetch_names(&cfg, &badges),
    )
    .await;
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

/// `true` for the two git builtins still backed by a subprocess (`git_status`/
/// `git_counts`): their cache entries get request-time fingerprint invalidation (see
/// `handle_get`) instead of waiting out `interval`, because unlike the other git builtins
/// they're too expensive to just recompute in-process every request.
const fn needs_fingerprint(source: &Source) -> bool {
    matches!(source, Source::Builtin { name, .. } if name.uses_porcelain())
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

/// A path-scoped badge's resolved root (`None` = render empty, spawn nothing: no root found
/// or `when_file` unmet) and its `watch` fingerprint.
struct Resolved {
    root: Option<PathBuf>,
    watch: Option<cache::WatchFp>,
}

#[derive(Default)]
struct GitSnapshot {
    fingerprint: Option<provider::GitFingerprint>,
    immediate: BTreeMap<String, String>,
    /// Every requested non-global badge.
    scoped: BTreeMap<String, Resolved>,
}

fn resolve_scope(badge: &crate::config::BadgeConfig, git: Option<&Path>, cwd: &Path) -> Resolved {
    let root = match badge.scope {
        Scope::Global => None,
        Scope::GitRoot => git.map(Path::to_path_buf),
        Scope::ProjectRoot => {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            provider::project_root(cwd, &badge.path.markers, home.as_deref())
        }
    }
    .filter(|r| {
        badge.path.when_file.is_empty() || badge.path.when_file.iter().any(|f| r.join(f).exists())
    });
    let watch = root
        .as_ref()
        .filter(|_| !badge.path.watch.is_empty())
        .map(|r| {
            badge
                .path
                .watch
                .iter()
                .map(|w| file_id(&r.join(w)))
                .collect()
        });
    Resolved { root, watch }
}

/// One bounded job performs all request-time git reads. This also avoids repeatedly
/// statting HEAD/index when several git-count badges are requested together.
fn git_snapshot(cfg: &Config, cwd: &Path, badges: &[String]) -> GitSnapshot {
    let root = provider::git_root(cwd);
    // Resolved once per request, shared by every in-process git badge and the fingerprint.
    let dir = root.as_deref().and_then(provider::git_dir);
    let mut immediate = BTreeMap::new();
    let mut scoped = BTreeMap::new();
    let mut fingerprinted = false;
    for name in badges {
        let Some(badge) = cfg.badge.get(name) else {
            continue;
        };
        if badge.scope != Scope::Global {
            scoped.insert(name.clone(), resolve_scope(badge, root.as_deref(), cwd));
        }
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
        fingerprint,
        immediate,
        scoped,
    }
}

/// A requested badge whose value was just cold-started: its fetch signals `rx` when done.
struct ColdWaiter {
    name: String,
    key: cache::Key,
    rx: oneshot::Receiver<()>,
}

/// Runs the request-time filesystem job for path-scoped badges. `None` = it timed out (or
/// workers are saturated), distinct from a snapshot finding no repo.
async fn resolve_snapshot(
    state: &Daemon,
    cfg: &Arc<Config>,
    cwd: Option<&str>,
    badges: &[String],
    deadline: tokio::time::Instant,
) -> Option<GitSnapshot> {
    let needs_repo = badges.iter().any(|name| {
        cfg.badge
            .get(name)
            .is_some_and(|b| b.scope != Scope::Global)
    });
    let (true, Some(cwd)) = (needs_repo, cwd) else {
        return Some(GitSnapshot::default());
    };
    let cwd = PathBuf::from(cwd);
    let cfg = Arc::clone(cfg);
    let badges = badges.to_vec();
    state
        .providers
        .blocking
        .run(deadline, move || git_snapshot(&cfg, &cwd, &badges))
        .await
}

/// Last good value of the nearest cached root at or above `cwd`, for when the snapshot
/// timed out. ponytail: first env variant wins; a linear scan is fine for `max_paths` keys.
fn stale_value(state: &Daemon, name: &str, cwd: &Path) -> String {
    let cache = state.cache.lock().unwrap();
    cache
        .iter()
        .filter(|(k, _)| k.badge == name)
        .filter_map(|(k, e)| Some((k.root.as_deref().filter(|r| cwd.starts_with(r))?, e)))
        .max_by_key(|(r, _)| r.as_os_str().len())
        .and_then(|(_, e)| e.value.clone())
        .unwrap_or_default()
}

/// Touches the badge's cache entry and decides whether a fetch is due: returns the plan,
/// the entry's identity and its current value.
fn plan_badge(
    state: &Daemon,
    max_paths: usize,
    key: &cache::Key,
    now: Instant,
    interval: Duration,
    fp: Option<provider::GitFingerprint>,
    watch: Option<&cache::WatchFp>,
) -> (cache::FetchPlan, Arc<()>, Option<String>) {
    let mut cache = state.cache.lock().unwrap();
    let is_new_path_scoped = key.is_scoped() && !cache.contains_key(key);
    let entry = cache
        .entry(key.clone())
        .or_insert_with(|| cache::Entry::new(now));
    let plan = cache::plan(entry, now, interval, fp, watch);
    let identity = Arc::clone(&entry.identity);
    let current_value = entry.value.clone();
    // Enforce the path-scoped cap (LRU by last_access) right away: only the timer
    // path ran this before, so a config with only path-scoped (e.g. git) badges
    // could exceed it for up to `idle_exit`.
    if is_new_path_scoped {
        cache::enforce_path_scoped_cap(&mut cache, max_paths);
    }
    drop(cache);
    (plan, identity, current_value)
}

/// Waits (within `deadline`) for the cold fetches just started, then reads their values.
async fn wait_cold(
    state: &Daemon,
    cfg: &Config,
    deadline: tokio::time::Instant,
    mut cold_waiters: Vec<ColdWaiter>,
    values: &mut BTreeMap<String, String>,
) {
    let wait_all = async {
        for w in &mut cold_waiters {
            let _ = (&mut w.rx).await;
        }
    };
    let wait_deadline = deadline.min(tokio::time::Instant::now() + cfg.daemon.cold_wait);
    let _ = tokio::time::timeout_at(wait_deadline, wait_all).await;
    let cache = state.cache.lock().unwrap();
    for w in cold_waiters {
        if let Some(entry) = cache.get(&w.key) {
            values.insert(w.name, entry.value.clone().unwrap_or_default());
        }
    }
}

/// Answers a `get`: cache hit returns the last good value immediately (stale-while-
/// revalidate); a stale/missing value triggers a background refresh; a truly cold key (no
/// value at all) waits up to the configured `cold_wait` for the fetch it just started. If
/// the filesystem snapshot times out, path-scoped badges serve their cached value as is.
pub(super) async fn handle_get(
    state: &Arc<Daemon>,
    cfg: &Arc<Config>,
    cwd: Option<&str>,
    env: Option<&[(&str, &str)]>,
    badges: &[String],
) -> BTreeMap<String, String> {
    let now = Instant::now();
    // Filesystem reads and cold fetch waits share one budget, rather than adding a
    // timeout per badge. Even cold_wait=0 permits bounded fresh in-process git reads.
    let deadline =
        tokio::time::Instant::now() + cfg.daemon.cold_wait.max(Duration::from_millis(20));
    let snapshot = resolve_snapshot(state, cfg, cwd, badges, deadline).await;
    let timed_out = snapshot.is_none();
    let snapshot = snapshot.unwrap_or_default();
    // Reload may have completed while the filesystem job was running. Do not create
    // old-config cache entries after that reload's invalidation pass.
    if !Arc::ptr_eq(&state.config.lock().unwrap(), cfg) {
        return badges
            .iter()
            .map(|name| (name.clone(), String::new()))
            .collect();
    }
    let on_ac = refresh_on_ac_if_due(state, now);
    maybe_evict_idle(state, cfg, now);

    let mut values = BTreeMap::new();
    let mut cold_waiters: Vec<ColdWaiter> = Vec::new();

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

        let is_path_scoped = badge_cfg.scope != Scope::Global;
        if is_path_scoped && timed_out {
            // Root unknown: serve what is cached, spawn nothing.
            let stale = cwd.map_or_else(String::new, |c| stale_value(state, name, Path::new(c)));
            values.insert(name.clone(), stale);
            continue;
        }
        let resolved = snapshot.scoped.get(name);
        let scope = resolved.and_then(|r| r.root.clone());
        if is_path_scoped && scope.is_none() {
            // No git repo / project root, or `when_file` unmet: render empty, no fetch at all.
            values.insert(name.clone(), String::new());
            continue;
        }
        let current_watch = resolved.and_then(|r| r.watch.as_ref());
        let key = cache::Key::new(name.as_str(), scope, select_env(&badge_cfg.path.env, env));
        let current_fp = needs_fingerprint(&badge_cfg.source)
            .then_some(snapshot.fingerprint)
            .flatten();

        let (plan, identity, current_value) = plan_badge(
            state,
            cfg.daemon.max_paths,
            &key,
            now,
            cache::effective_interval(badge_cfg, on_ac),
            current_fp,
            current_watch,
        );

        if plan.spawn {
            let done = plan.cold.then(|| {
                let (tx, rx) = oneshot::channel();
                cold_waiters.push(ColdWaiter {
                    name: name.clone(),
                    key: key.clone(),
                    rx,
                });
                tx
            });
            spawn_fetch(Arc::clone(state), key, identity, Arc::clone(cfg), done);
        }

        values.insert(name.clone(), current_value.unwrap_or_default());
    }

    if !cold_waiters.is_empty() {
        wait_cold(state, cfg, deadline, cold_waiters, &mut values).await;
    }

    values
}

fn build_status(state: &Daemon) -> BTreeMap<String, String> {
    let now = Instant::now();
    let last_error = state.config_meta.lock().unwrap().last_error.clone();
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
            .root
            .as_ref()
            .map_or_else(|| "-".to_string(), |p| p.display().to_string());
        let age = fetched_at.map_or_else(
            || "-".to_string(),
            |t| now.saturating_duration_since(t).as_secs().to_string(),
        );
        lines.push(format!(
            "{:<20} {:<40} {:>8} {:>7}",
            key.badge, scope, age, errors
        ));
    }

    if let Some(e) = last_error {
        lines.push(format!("config error: {e}"));
    }
    let mut values = BTreeMap::new();
    values.insert("status".to_string(), lines.join("\n"));
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tests::{test_cfg, test_daemon};
    use crate::template::Dialect;
    use crate::test_support::TempDir;

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
        let values = handle_get(&state, &cfg, None, None, &req).await;
        assert_eq!(values.len(), 2);
        assert_eq!(values["nope"], "");
        assert_eq!(state.cache.lock().unwrap().len(), 1);
    }

    /// `handle_get`'s staleness check must use the battery-aware `effective_interval` (same
    /// as the timer path), not always `badge_cfg.interval`. The entry is seeded directly so
    /// nothing depends on a real fetch or on sleeping; the warm path has no `.await`, so the
    /// spawned refresh cannot run (or clear `in_flight`) before the assertion.
    #[tokio::test]
    async fn handle_get_uses_battery_interval_when_on_battery() {
        let cfg = Arc::new(test_cfg(Duration::from_secs(10), Duration::from_millis(10)));
        let key = cache::Key::global("b");
        for (on_ac, expect) in [(false, true), (true, false)] {
            let state = test_daemon(on_ac);
            *state.config.lock().unwrap() = Arc::clone(&cfg);
            let mut entry = cache::Entry::new(Instant::now());
            entry.value = Some("v".into());
            entry.fetched_at = Instant::now().checked_sub(Duration::from_millis(50));
            state.cache.lock().unwrap().insert(key.clone(), entry);

            handle_get(&state, &cfg, None, None, &["b".to_string()]).await;
            assert_eq!(state.cache.lock().unwrap()[&key].in_flight, expect);
        }
    }

    /// `cat .nvmrc` badge scoped to the nearest `.nvmrc` ancestor of the request cwd.
    fn project_cfg(when_file: &[&str]) -> Arc<Config> {
        let mut cfg = test_cfg(Duration::from_secs(3600), Duration::from_secs(3600));
        let b = cfg.badge.get_mut("b").unwrap();
        b.source = Source::Command {
            command: "cat .nvmrc".into(),
            args: None,
        };
        b.scope = Scope::ProjectRoot;
        b.path = crate::config::PathOpts {
            markers: vec![".nvmrc".into()],
            watch: vec![".nvmrc".into()],
            when_file: when_file.iter().map(ToString::to_string).collect(),
            env: Vec::new(),
        };
        Arc::new(cfg)
    }

    #[tokio::test]
    async fn watch_change_refreshes_project_root_value() {
        let tmp = TempDir::new("watch");
        let dir = tmp.path();
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join(".nvmrc"), "v18").unwrap();
        let cfg = project_cfg(&[]);
        let state = test_daemon(true);
        *state.config.lock().unwrap() = Arc::clone(&cfg);
        let cwd = sub.to_str().unwrap();
        let req = ["b".to_string()];

        // interval is 1h, so only the watch fingerprint can trigger the second fetch.
        assert_eq!(
            handle_get(&state, &cfg, Some(cwd), None, &req).await["b"],
            "v18"
        );
        std::fs::write(dir.join(".nvmrc"), "v20.1").unwrap();
        assert_eq!(
            handle_get(&state, &cfg, Some(cwd), None, &req).await["b"],
            "v20.1"
        );
        // Keyed by the project root, not the cwd.
        assert!(state.cache.lock().unwrap().contains_key(&cache::Key::new(
            "b",
            Some(dir.to_path_buf()),
            None
        )));
    }

    #[tokio::test]
    async fn when_file_unmet_or_no_root_renders_empty_without_spawning() {
        let tmp = TempDir::new("whenfile");
        let dir = tmp.path();
        std::fs::write(dir.join(".nvmrc"), "v18").unwrap();
        let state = test_daemon(true);
        let req = ["b".to_string()];

        let cfg = project_cfg(&["package.json"]);
        *state.config.lock().unwrap() = Arc::clone(&cfg);
        let cwd = dir.to_str().unwrap();
        assert_eq!(
            handle_get(&state, &cfg, Some(cwd), None, &req).await["b"],
            ""
        );
        assert!(state.cache.lock().unwrap().is_empty());

        std::fs::write(dir.join("package.json"), "{}").unwrap();
        assert_eq!(
            handle_get(&state, &cfg, Some(cwd), None, &req).await["b"],
            "v18"
        );

        // No ancestor holds a marker (cwd has none; `/` is the walk's end).
        let empty = project_cfg(&[]);
        *state.config.lock().unwrap() = Arc::clone(&empty);
        state.cache.lock().unwrap().clear();
        std::fs::remove_file(dir.join(".nvmrc")).unwrap();
        assert_eq!(
            handle_get(&state, &empty, Some(cwd), None, &req).await["b"],
            ""
        );
        assert!(state.cache.lock().unwrap().is_empty());
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn env_glob_matches_exact_names_and_trailing_star_prefixes() {
        assert!(crate::config::env_matches("PATH", "PATH"));
        assert!(!crate::config::env_matches("PATH", "PATHX"));
        assert!(!crate::config::env_matches("PATH", "MYPATH"));
        assert!(crate::config::env_matches("MISE_*", "MISE_SHELL"));
        assert!(crate::config::env_matches("MISE_*", "MISE_"));
        assert!(!crate::config::env_matches("MISE_*", "XMISE_A"));
        let env = [("Z", "1"), ("MISE_B", "2"), ("MISE_A", "3"), ("OTHER", "4")];
        let pats = ["MISE_*".to_string(), "Z".to_string()];
        assert_eq!(
            select_env(&pats, Some(&env)),
            Some(pairs(&[("MISE_A", "3"), ("MISE_B", "2"), ("Z", "1")]))
        );
        // Client sent env but nothing matches: `Some(empty)`, distinct from no env at all.
        assert_eq!(select_env(&["NOPE".into()], Some(&env)), Some(vec![]));
        assert_eq!(select_env(&pats, None), None);
        assert_eq!(select_env(&[], Some(&env)), None);
    }

    #[test]
    fn parse_get_reads_old_and_env_formats() {
        let (ver, cwd, badges, env) = parse_get("get\u{1f}1.0\u{1f}/p\u{1f}a\u{1f}b");
        assert_eq!((ver, cwd), ("1.0", "/p"));
        assert_eq!(badges, ["a", "b"]);
        assert_eq!(env, None);

        let (_, _, badges, env) = parse_get(
            "get\u{1f}1.0\u{1f}/p\u{1f}a\u{1f}\u{1e}env\u{1f}PATH=/x:/y\u{1f}K=v=w\u{1f}junk",
        );
        assert_eq!(badges, ["a"]);
        assert_eq!(env, Some(vec![("PATH", "/x:/y"), ("K", "v=w")]));
        let (_, _, _, env) = parse_get("get\u{1f}1.0\u{1f}/p\u{1f}a\u{1f}\u{1e}env");
        assert_eq!(env, Some(vec![]));
    }

    /// Global badge running `script` with `env` patterns; returns a getter over client envs.
    fn env_badge(script: &str, patterns: &[&str]) -> (Arc<Daemon>, Arc<Config>) {
        let mut cfg = test_cfg(Duration::from_secs(3600), Duration::from_secs(3600));
        let b = cfg.badge.get_mut("b").unwrap();
        b.source = Source::Command {
            command: script.into(),
            args: None,
        };
        b.path.env = patterns.iter().map(ToString::to_string).collect();
        let cfg = Arc::new(cfg);
        let state = test_daemon(true);
        *state.config.lock().unwrap() = Arc::clone(&cfg);
        (state, cfg)
    }

    async fn get_b(state: &Arc<Daemon>, cfg: &Arc<Config>, env: Option<&[(&str, &str)]>) -> String {
        handle_get(state, cfg, None, env, &["b".to_string()]).await["b"].clone()
    }

    #[tokio::test]
    async fn command_sees_forwarded_env_and_cache_key_follows_its_value() {
        let (state, cfg) = env_badge("printf %s \"$STFG_TEST_FOO\"", &["STFG_TEST_*"]);
        let one = [("STFG_TEST_FOO", "one"), ("NOT_LISTED", "x")];
        assert_eq!(get_b(&state, &cfg, Some(&one)).await, "one");
        let two = [("STFG_TEST_FOO", "two")];
        assert_eq!(get_b(&state, &cfg, Some(&two)).await, "two");
        // Old client (no env block): daemon env (which lacks the variable), its own entry.
        assert_eq!(get_b(&state, &cfg, None).await, "");
        let cache = state.cache.lock().unwrap();
        assert_eq!(cache.len(), 3);
        let key = cache::Key::new("b", None, Some(pairs(&[("STFG_TEST_FOO", "one")])));
        assert_eq!(cache[&key].value.as_deref(), Some("one"));
        assert!(key.is_scoped(), "env-keyed entries are LRU-evictable");
    }

    #[tokio::test]
    async fn client_missing_a_variable_does_not_inherit_the_daemons() {
        let (state, cfg) = env_badge("printf %s \"${HOME-unset}\"", &["HOME"]);
        let daemon_home = std::env::var("HOME").unwrap_or_else(|_| "unset".into());
        // The client has no HOME (e.g. `deactivate`d VIRTUAL_ENV): not the daemon's.
        let other = [("OTHER", "x")];
        assert_eq!(get_b(&state, &cfg, Some(&other)).await, "unset");
        let home = [("HOME", "/h")];
        assert_eq!(get_b(&state, &cfg, Some(&home)).await, "/h");
        // No env block: the daemon's env, under a different key than "env sent, none matched".
        assert_eq!(get_b(&state, &cfg, None).await, daemon_home);
        assert_eq!(state.cache.lock().unwrap().len(), 3);
    }

    #[test]
    fn status_shows_last_config_error() {
        let state = test_daemon(true);
        assert!(!build_status(&state)["status"].contains("config error"));
        state.config_meta.lock().unwrap().last_error = Some("boom".into());
        assert!(build_status(&state)["status"].ends_with("config error: boom"));
    }

    #[tokio::test]
    async fn snapshot_timeout_serves_nearest_cached_value_not_blank() {
        let tmp = TempDir::new("stale");
        let dir = tmp.path();
        std::fs::write(dir.join(".nvmrc"), "v18").unwrap();
        let cfg = project_cfg(&[]);
        let state = test_daemon(true);
        *state.config.lock().unwrap() = Arc::clone(&cfg);
        let mut entry = cache::Entry::new(Instant::now());
        entry.value = Some("cached".into());
        let key = cache::Key::new("b", Some(dir.to_path_buf()), None);
        state.cache.lock().unwrap().insert(key, entry);

        let sub = dir.join("sub");
        assert_eq!(stale_value(&state, "b", &sub), "cached");
        assert_eq!(stale_value(&state, "b", Path::new("/elsewhere")), "");
        // Sibling prefix: `/a/b` must not match `/a/bc`.
        let sibling = PathBuf::from(format!("{}c", dir.display()));
        assert_eq!(stale_value(&state, "b", &sibling), "");
        assert_eq!(stale_value(&state, "other", &sub), "");

        // An expired deadline is a timeout (`None`), not a snapshot that found no repo.
        let past = tokio::time::Instant::now() - Duration::from_millis(1);
        let req = ["b".to_string()];
        let cwd = sub.to_str();
        assert!(
            resolve_snapshot(&state, &cfg, cwd, &req, past)
                .await
                .is_none()
        );
        let far = tokio::time::Instant::now() + Duration::from_secs(5);
        assert!(
            resolve_snapshot(&state, &cfg, cwd, &req, far)
                .await
                .is_some()
        );
    }

    async fn shutdown_requested(s: &Daemon) -> bool {
        tokio::time::timeout(Duration::from_millis(50), s.shutdown.notified())
            .await
            .is_ok()
    }

    /// Runs `handle_connection` on one end of a socketpair with `request`, returns the reply.
    async fn exchange(state: &Arc<Daemon>, request: &[u8]) -> String {
        use tokio::io::AsyncReadExt as _;
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(request).await.unwrap();
        let task = tokio::spawn(handle_connection(Arc::clone(state), server));
        let mut out = String::new();
        tokio::time::timeout(Duration::from_secs(3), client.read_to_string(&mut out))
            .await
            .unwrap()
            .unwrap();
        task.await.unwrap();
        out
    }

    #[tokio::test]
    async fn connection_dispatches_on_first_byte() {
        let state = test_daemon(true);

        // Empty line (EOF without newline) and unknown first byte: dropped, no reply.
        assert_eq!(exchange(&state, b"").await, "");
        assert_eq!(exchange(&state, b"x\n").await, "");
        assert!(!shutdown_requested(&state).await);
        // `{`: JSON admin. Unknown cmd -> error response; stop -> notifies shutdown.
        let unknown = exchange(&state, b"{\"v\":1,\"cmd\":\"nope\"}\n").await;
        assert!(unknown.contains("\"ok\":false"), "{unknown}");
        assert!(
            exchange(&state, b"{\"v\":1,\"cmd\":\"reload\"}\n")
                .await
                .contains("\"ok\":true")
        );
        assert!(!shutdown_requested(&state).await);
        assert!(
            exchange(&state, b"{\"v\":1,\"cmd\":\"stop\"}\n")
                .await
                .contains("\"ok\":true")
        );
        assert!(shutdown_requested(&state).await);
        // `g`: text get; unknown badge -> one empty line.
        let ver = env!("CARGO_PKG_VERSION");
        let req = format!("get\u{1f}{ver}\u{1f}/\u{1f}nope\n");
        assert_eq!(exchange(&state, req.as_bytes()).await, "\n");
        assert!(!shutdown_requested(&state).await);
    }

    #[tokio::test]
    async fn version_mismatch_replies_then_shuts_down() {
        let state = test_daemon(true);
        let reply = exchange(&state, b"get\x1f0.0.0-old\x1f/\x1fnope\n").await;
        assert_eq!(reply, "\n");
        // notify_one stores a permit, so this resolves immediately.
        tokio::time::timeout(Duration::from_millis(50), state.shutdown.notified())
            .await
            .unwrap();
    }
}
