//! Answering a `get`: one bounded filesystem snapshot (scope roots, in-process git values,
//! fingerprints), then per badge a cache lookup that may start a fetch, then a short wait for
//! cold keys. Restarts under the new config if a reload lands mid-request.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use super::cache;
use super::reload::file_id;
use super::render::{fetch_names, render_raw};
use super::{Daemon, maybe_evict_idle, refresh_power_if_due, spawn_fetch};
use crate::config::{Config, Scope, Source};
use crate::provider;

/// The client's variables, borrowed from the request line.
pub(super) type ClientEnv<'a> = Option<Vec<(&'a str, &'a str)>>;

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

/// `true` for the two git builtins still backed by a subprocess (`git_status`/
/// `git_counts`): their cache entries get request-time fingerprint invalidation (see
/// `handle_get`) instead of waiting out `interval`, because unlike the other git builtins
/// they're too expensive to just recompute in-process every request.
const fn needs_fingerprint(source: &Source) -> bool {
    matches!(source, Source::Builtin { name, .. } if name.uses_porcelain())
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
        .path_blocking
        .run(deadline, move || git_snapshot(&cfg, &cwd, &badges))
        .await
        .ok()
}

/// Last good value of the nearest cached root at or above `cwd`, for when the snapshot
/// timed out. ponytail: first env variant wins; a linear scan is fine for `max_paths` keys.
fn stale_value(state: &Daemon, name: &str, cwd: &Path) -> String {
    state
        .core()
        .cache
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
    let mut core = state.core();
    let cache = &mut core.cache;
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
        cache::enforce_path_scoped_cap(cache, max_paths);
    }
    drop(core);
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
    let core = state.core();
    for w in cold_waiters {
        if let Some(entry) = core.cache.get(&w.key) {
            values.insert(w.name, entry.value.clone().unwrap_or_default());
        }
    }
}

/// A `get`'s values, and the config they were resolved under (`response_lines` joins groups
/// with it).
pub(super) struct Answer {
    pub cfg: Arc<Config>,
    pub values: BTreeMap<String, String>,
}

/// Resolutions per request before it answers with whatever the last one produced: each retry
/// needs a reload to land during the previous one, and reloads are at least `config_check`
/// apart.
const MAX_ATTEMPTS: usize = 3;

/// Answers a `get` for badge and group names: cache hit returns the last good value
/// immediately (stale-while-revalidate); a stale/missing value triggers a background refresh;
/// a truly cold key (no value at all) waits up to the configured `cold_wait` for the fetch it
/// just started. If the filesystem snapshot times out, path-scoped badges serve their cached
/// value as is. A reload landing mid-request restarts resolution under the new config, within
/// the same deadline, instead of answering blank.
pub(super) async fn handle_get(
    state: &Arc<Daemon>,
    cwd: Option<&str>,
    env: Option<&[(&str, &str)]>,
    requested: &[String],
) -> Answer {
    let mut cfg = state.config();
    // Filesystem reads and cold fetch waits share one budget, rather than adding a
    // timeout per badge. Even cold_wait=0 permits bounded fresh in-process git reads.
    let deadline =
        tokio::time::Instant::now() + cfg.daemon.cold_wait.max(Duration::from_millis(20));
    let mut attempt = 1;
    loop {
        let badges = fetch_names(&cfg, requested);
        match resolve(state, &cfg, cwd, env, &badges, deadline).await {
            Outcome::Reloaded(_) if attempt < MAX_ATTEMPTS => {
                attempt += 1;
                cfg = state.config();
            }
            Outcome::Done(values) | Outcome::Reloaded(values) => return Answer { cfg, values },
        }
    }
}

/// How one resolution under a fixed config ended.
enum Outcome {
    Done(BTreeMap<String, String>),
    /// The config was replaced meanwhile: these values (blank if the cache wasn't reached
    /// yet) may not match the new one.
    Reloaded(BTreeMap<String, String>),
}

/// One resolution of `badges` (groups already expanded) under `cfg`.
async fn resolve(
    state: &Arc<Daemon>,
    cfg: &Arc<Config>,
    cwd: Option<&str>,
    env: Option<&[(&str, &str)]>,
    badges: &[String],
    deadline: tokio::time::Instant,
) -> Outcome {
    let now = Instant::now();
    let snapshot = resolve_snapshot(state, cfg, cwd, badges, deadline).await;
    let timed_out = snapshot.is_none();
    let snapshot = snapshot.unwrap_or_default();
    // Reload may have completed while the filesystem job was running. Do not create
    // old-config cache entries after that reload's invalidation pass. No `.await` from here
    // to the cold wait, so no reload can land in between.
    if !Arc::ptr_eq(&state.config(), cfg) {
        return Outcome::Reloaded(
            badges
                .iter()
                .map(|name| (name.clone(), String::new()))
                .collect(),
        );
    }
    let power = refresh_power_if_due(state, now);
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
            cache::effective_interval(badge_cfg, power),
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
        if !Arc::ptr_eq(&state.config(), cfg) {
            return Outcome::Reloaded(values);
        }
    }

    Outcome::Done(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tests::{test_cfg, test_daemon};
    use crate::test_support::TempDir;

    #[tokio::test]
    async fn handle_get_duplicate_names_fetch_once() {
        let cfg = Arc::new(test_cfg(Duration::from_secs(10), Duration::from_secs(10)));
        let state = test_daemon(cache::Power::Ac);
        state.set_config(&cfg);
        let req = ["b", "b", "nope"].map(String::from);
        let values = handle_get(&state, None, None, &req).await.values;
        assert_eq!(values.len(), 2);
        assert_eq!(values["nope"], "");
        assert_eq!(state.core().cache.len(), 1);
    }

    /// `handle_get`'s staleness check must use the battery-aware `effective_interval` (same
    /// as the timer path), not always `badge_cfg.interval`. The entry is seeded directly so
    /// nothing depends on a real fetch or on sleeping; the warm path has no `.await`, so the
    /// spawned refresh cannot run (or clear `in_flight`) before the assertion.
    #[tokio::test]
    async fn handle_get_uses_battery_interval_when_on_battery() {
        let cfg = Arc::new(test_cfg(Duration::from_secs(10), Duration::from_millis(10)));
        let key = cache::Key::global("b");
        for (power, expect) in [(cache::Power::Battery, true), (cache::Power::Ac, false)] {
            let state = test_daemon(power);
            state.set_config(&cfg);
            let mut entry = cache::Entry::new(Instant::now());
            entry.value = Some("v".into());
            entry.fetched_at = Instant::now().checked_sub(Duration::from_millis(50));
            state.insert(&key, entry);

            handle_get(&state, None, None, &["b".to_string()]).await;
            assert_eq!(state.entry(&key, |e| e.in_flight), expect);
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

    /// A reload landing while the snapshot runs (the swap below is spawned, so it runs at the
    /// snapshot's `.await`) used to answer blank; now the request restarts under the new
    /// config.
    #[tokio::test]
    async fn reload_during_snapshot_retries_instead_of_answering_blank() {
        let tmp = TempDir::new("reload-race");
        let dir = tmp.path();
        std::fs::write(dir.join(".nvmrc"), "v18").unwrap();
        let state = test_daemon(cache::Power::Ac);
        state.set_config(&project_cfg(&[]));
        let reloaded = project_cfg(&[]);
        let swap = {
            let (state, reloaded) = (Arc::clone(&state), Arc::clone(&reloaded));
            tokio::spawn(async move { state.set_config(&reloaded) })
        };
        let answer = handle_get(&state, dir.to_str(), None, &["b".to_string()]).await;
        assert!(swap.is_finished());
        assert!(
            Arc::ptr_eq(&answer.cfg, &reloaded),
            "answered under the new config"
        );
        assert_eq!(answer.values["b"], "v18");
    }

    #[tokio::test]
    async fn watch_change_refreshes_project_root_value() {
        let tmp = TempDir::new("watch");
        let dir = tmp.path();
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join(".nvmrc"), "v18").unwrap();
        let cfg = project_cfg(&[]);
        let state = test_daemon(cache::Power::Ac);
        state.set_config(&cfg);
        let cwd = sub.to_str().unwrap();
        let req = ["b".to_string()];

        // interval is 1h, so only the watch fingerprint can trigger the second fetch.
        assert_eq!(
            handle_get(&state, Some(cwd), None, &req).await.values["b"],
            "v18"
        );
        std::fs::write(dir.join(".nvmrc"), "v20.1").unwrap();
        assert_eq!(
            handle_get(&state, Some(cwd), None, &req).await.values["b"],
            "v20.1"
        );
        // Keyed by the project root, not the cwd.
        assert!(state.core().cache.contains_key(&cache::Key::new(
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
        let state = test_daemon(cache::Power::Ac);
        let req = ["b".to_string()];

        let cfg = project_cfg(&["package.json"]);
        state.set_config(&cfg);
        let cwd = dir.to_str().unwrap();
        assert_eq!(
            handle_get(&state, Some(cwd), None, &req).await.values["b"],
            ""
        );
        assert!(state.core().cache.is_empty());

        std::fs::write(dir.join("package.json"), "{}").unwrap();
        assert_eq!(
            handle_get(&state, Some(cwd), None, &req).await.values["b"],
            "v18"
        );

        // No ancestor holds a marker (cwd has none; `/` is the walk's end).
        let empty = project_cfg(&[]);
        state.set_config(&empty);
        state.core().cache.clear();
        std::fs::remove_file(dir.join(".nvmrc")).unwrap();
        assert_eq!(
            handle_get(&state, Some(cwd), None, &req).await.values["b"],
            ""
        );
        assert!(state.core().cache.is_empty());
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

    /// A daemon whose global badge `b` runs `script` with `env` patterns.
    fn env_badge(script: &str, patterns: &[&str]) -> Arc<Daemon> {
        let mut cfg = test_cfg(Duration::from_secs(3600), Duration::from_secs(3600));
        let b = cfg.badge.get_mut("b").unwrap();
        b.source = Source::Command {
            command: script.into(),
            args: None,
        };
        b.path.env = patterns.iter().map(ToString::to_string).collect();
        let cfg = Arc::new(cfg);
        let state = test_daemon(cache::Power::Ac);
        state.set_config(&cfg);
        state
    }

    async fn get_b(state: &Arc<Daemon>, env: Option<&[(&str, &str)]>) -> String {
        handle_get(state, None, env, &["b".to_string()])
            .await
            .values["b"]
            .clone()
    }

    #[tokio::test]
    async fn command_sees_forwarded_env_and_cache_key_follows_its_value() {
        let state = env_badge("printf %s \"$STFG_TEST_FOO\"", &["STFG_TEST_*"]);
        let one = [("STFG_TEST_FOO", "one"), ("NOT_LISTED", "x")];
        assert_eq!(get_b(&state, Some(&one)).await, "one");
        let two = [("STFG_TEST_FOO", "two")];
        assert_eq!(get_b(&state, Some(&two)).await, "two");
        // Old client (no env block): daemon env (which lacks the variable), its own entry.
        assert_eq!(get_b(&state, None).await, "");
        let core = state.core();
        assert_eq!(core.cache.len(), 3);
        let key = cache::Key::new("b", None, Some(pairs(&[("STFG_TEST_FOO", "one")])));
        assert_eq!(core.cache[&key].value.as_deref(), Some("one"));
        assert!(key.is_scoped(), "env-keyed entries are LRU-evictable");
    }

    #[tokio::test]
    async fn client_missing_a_variable_does_not_inherit_the_daemons() {
        let state = env_badge("printf %s \"${HOME-unset}\"", &["HOME"]);
        let daemon_home = std::env::var("HOME").unwrap_or_else(|_| "unset".into());
        // The client has no HOME (e.g. `deactivate`d VIRTUAL_ENV): not the daemon's.
        let other = [("OTHER", "x")];
        assert_eq!(get_b(&state, Some(&other)).await, "unset");
        let home = [("HOME", "/h")];
        assert_eq!(get_b(&state, Some(&home)).await, "/h");
        // No env block: the daemon's env, under a different key than "env sent, none matched".
        assert_eq!(get_b(&state, None).await, daemon_home);
        assert_eq!(state.core().cache.len(), 3);
    }

    #[tokio::test]
    async fn snapshot_timeout_serves_nearest_cached_value_not_blank() {
        let tmp = TempDir::new("stale");
        let dir = tmp.path();
        std::fs::write(dir.join(".nvmrc"), "v18").unwrap();
        let cfg = project_cfg(&[]);
        let state = test_daemon(cache::Power::Ac);
        state.set_config(&cfg);
        let mut entry = cache::Entry::new(Instant::now());
        entry.value = Some("cached".into());
        let key = cache::Key::new("b", Some(dir.to_path_buf()), None);
        state.insert(&key, entry);

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
}
