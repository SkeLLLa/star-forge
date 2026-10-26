//! Config hot-reload: file identity stamps and the throttled reload check.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use super::cache::{Entry, Key};
use super::{Daemon, MAINTENANCE_BUDGET, set_config_error};
use crate::blocking::Rejected;
use crate::config::{Config, Scope};
pub(super) use crate::file_id::{FileId, file_id};

/// Identities of the config file and its `palette_from` file (`None` = missing/unset).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Stamp {
    pub config: Option<FileId>,
    pub palette: Option<FileId>,
}

pub(super) fn stamp(config: &Path, palette: Option<&Path>) -> Stamp {
    Stamp {
        config: file_id(config),
        palette: palette.and_then(file_id),
    }
}

/// Writes `cfg`'s `env` patterns where clients read them (`client::read_env_patterns`), so a
/// `get` carries only variables some badge can use. Through a temp file and a rename, so a
/// client never reads a torn file; a publish that outlives its budget and lands after a newer
/// one is discarded. Best effort: if the write fails the old file is removed (a client that
/// finds no usable file sends its whole environment, which the daemon filters per badge
/// anyway), and on any failure the next config check retries. A stale file left behind is
/// harmless: its tag no longer matches, so the daemon uses its own environment.
pub(super) async fn publish_env_patterns(state: &Daemon, cfg: Arc<Config>) {
    let (path, generation, published_generation) = {
        let mut meta = state.config_meta.lock().expect("config_meta lock poisoned");
        let Some(path) = meta.env_patterns.clone() else {
            return;
        };
        meta.publish_generation += 1;
        (
            path,
            meta.publish_generation,
            Arc::clone(&meta.published_generation),
        )
    };
    let written = state
        .providers
        .blocking
        .run(
            tokio::time::Instant::now() + MAINTENANCE_BUDGET,
            move || {
                // Validation rejects control characters in `env`, so each pattern is one line.
                let mut text = format!("{}\n", crate::client::ENV_PATTERNS_HEADER);
                for p in cfg.env_patterns() {
                    text.push_str(p);
                    text.push('\n');
                }
                let tmp = path.with_extension(format!("tmp.{generation}"));
                let wrote = std::fs::write(&tmp, text).is_ok();
                let mut published = published_generation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if *published > generation {
                    let _ = std::fs::remove_file(&tmp);
                    return false;
                }
                *published = generation;
                let ok = wrote && std::fs::rename(&tmp, &path).is_ok();
                if !ok {
                    let _ = std::fs::remove_file(&tmp);
                    let _ = std::fs::remove_file(&path);
                }
                drop(published);
                ok
            },
        )
        .await;
    state
        .config_meta
        .lock()
        .expect("config_meta lock poisoned")
        .env_patterns_pending = written != Ok(true);
}

/// Whether [`check_reload`] honours the `config_check` throttle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReloadCheck {
    /// Timer- or request-driven: skipped if the file was stat'ed within `config_check`.
    Throttled,
    /// An explicit `reload` command: always stats the file.
    Forced,
}

/// Stats the config file at most once every `config_check` (bypassed by an explicit
/// `reload` command) and reloads if it, or its `palette_from` file, changed (inode, size or ctime). A config that fails to parse or
/// validate is logged and ignored; the previous config keeps running.
pub(super) async fn check_reload(state: &Arc<Daemon>, check: ReloadCheck, now: Instant) {
    let (config_check, palette) = {
        let cfg = state.config();
        (cfg.daemon.config_check, cfg.daemon.palette_from.clone())
    };
    let deadline = tokio::time::Instant::now() + MAINTENANCE_BUDGET;
    let (path, previous) = loop {
        let notified = state.reload_finished.notified();
        let ready = {
            let mut meta = state.config_meta.lock().expect("config_meta lock poisoned");
            if check == ReloadCheck::Throttled
                && now.saturating_duration_since(meta.last_checked) < config_check
            {
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
        if check == ReloadCheck::Throttled
            || tokio::time::timeout_at(deadline, notified).await.is_err()
        {
            return;
        }
    };
    let result = state
        .providers
        .blocking
        .run(deadline, move || {
            let pre = stamp(&path, palette.as_deref());
            if check == ReloadCheck::Throttled && previous == pre {
                return None;
            }
            let cfg = Config::load(&path);
            // Stamped before the load so an edit during it is seen next check. Limitation: if
            // `palette_from` changed, the new file is stat'ed after the load (tiny window).
            let st = match &cfg {
                Ok(c) if c.daemon.palette_from.as_deref() != palette.as_deref() => Stamp {
                    palette: c.daemon.palette_from.as_deref().and_then(file_id),
                    ..pre
                },
                _ => pre,
            };
            Some((st, cfg))
        })
        .await;
    // Set by a successful reload; published once every lock is released.
    let mut published = None;
    match result {
        Ok(Some((st, Ok(new_cfg)))) => {
            let mut meta = state.config_meta.lock().expect("config_meta lock poisoned");
            meta.stamp = st;
            meta.last_error = None;
            drop(meta);
            let new_cfg = Arc::new(new_cfg);
            let mut core = state.core();
            let old_cfg = std::mem::replace(&mut core.config, Arc::clone(&new_cfg));
            core.cache
                .retain(|key, entry| retain_entry(&old_cfg, &new_cfg, key, entry));
            drop(core);
            state.reschedule.notify_one();
            published = Some(new_cfg);
        }
        Ok(Some((_, Err(e)))) => {
            let msg = format!("config reload failed, keeping previous config: {e}");
            set_config_error(
                &mut state.config_meta.lock().expect("config_meta lock poisoned"),
                msg,
            );
        }
        Err(why) => {
            let msg = match why {
                Rejected::Saturated => "config check skipped: filesystem workers are busy",
                Rejected::Expired => "config check timed out",
                Rejected::Panicked => "config check panicked",
            }
            .into();
            set_config_error(
                &mut state.config_meta.lock().expect("config_meta lock poisoned"),
                msg,
            );
        }
        // Unchanged stamp: a failed parse never stores it, so any old error was transient.
        Ok(None) => {
            state
                .config_meta
                .lock()
                .expect("config_meta lock poisoned")
                .last_error = None;
        }
    }
    // A failed earlier publish is retried with the running config.
    if published.is_none()
        && state
            .config_meta
            .lock()
            .expect("config_meta lock poisoned")
            .env_patterns_pending
    {
        published = Some(state.config());
    }
    // Before clearing `checking`: the next reload can't publish until this one has.
    if let Some(cfg) = published {
        publish_env_patterns(state, cfg).await;
    }
    state
        .config_meta
        .lock()
        .expect("config_meta lock poisoned")
        .checking = false;
    state.reload_finished.notify_waiters();
}

/// Whether a reload keeps `key`'s entry, resetting it if its badge changed. An unchanged
/// badge's entry is left as is. A changed one keeps its activity and last good value but is
/// refreshed under the new config; its `in_flight` flag stays, so a fetch still running for it
/// is not duplicated, and that fetch's result is discarded (see `spawn_fetch`).
fn retain_entry(old: &Config, new: &Config, key: &Key, entry: &mut Entry) -> bool {
    let name = &key.badge;
    let Some(badge) = new.badge.get(name) else {
        return false;
    };
    if key.root.is_some() != (badge.scope != Scope::Global) {
        return false;
    }
    let previous = old.badge.get(name);
    if previous == Some(badge) {
        return true;
    }
    // Scoped keys embed the old root/env: if what selects them changed, they can never be
    // hit again (or hold a value computed another way).
    if key.is_scoped()
        && previous.is_none_or(|old| {
            old.source != badge.source || old.scope != badge.scope || old.path != badge.path
        })
    {
        return false;
    }
    entry.fetched_at = None;
    entry.fingerprint = None;
    entry.watch = None;
    entry.errors = 0;
    entry.next_attempt = Instant::now();
    true
}

/// Implicit checks must not consume the prompt's latency budget.
pub(super) fn schedule_reload_check(state: &Arc<Daemon>, now: Instant) {
    let config_check = state.config().daemon.config_check;
    let due = {
        let meta = state.config_meta.lock().expect("config_meta lock poisoned");
        !meta.checking && now.saturating_duration_since(meta.last_checked) >= config_check
    };
    if due {
        let state = Arc::clone(state);
        tokio::spawn(async move { check_reload(&state, ReloadCheck::Throttled, now).await });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::cache;
    use crate::daemon::tests::test_daemon;
    use crate::template::Dialect;
    use crate::test_support::TempDir;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    #[test]
    fn stamp_tracks_both_files() {
        let tmp = TempDir::new("stamp");
        let dir = tmp.path();
        let (c, p) = (dir.join("c.toml"), dir.join("p.toml"));
        assert_eq!(stamp(&c, Some(&p)), Stamp::default());
        std::fs::write(&c, "").unwrap();
        let s1 = stamp(&c, Some(&p));
        assert!(s1.config.is_some() && s1.palette.is_none());
        std::fs::write(&p, "").unwrap();
        let s2 = stamp(&c, Some(&p));
        assert_ne!(s1, s2);
        assert_eq!(stamp(&c, None).palette, None);
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
    }

    #[tokio::test]
    async fn failed_env_patterns_publish_is_retried_by_the_next_check() {
        let tmp = TempDir::new("publish");
        let dir = tmp.path().join("run");
        let file = dir.join("env-patterns");
        let state = test_daemon(cache::Power::Ac);
        state.config_meta.lock().unwrap().env_patterns = Some(file.clone());
        publish_env_patterns(&state, state.config()).await;
        assert!(state.config_meta.lock().unwrap().env_patterns_pending);

        std::fs::create_dir(&dir).unwrap();
        check_reload(&state, ReloadCheck::Forced, Instant::now()).await;
        assert!(!state.config_meta.lock().unwrap().env_patterns_pending);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with(crate::client::ENV_PATTERNS_HEADER));
        let leftovers = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(leftovers, 1, "temp files are renamed or removed");
    }

    /// Temp dir with a config (`c.toml`, group `r` = `[{a}](brand)`) and palette (`p.toml`).
    struct ReloadEnv {
        tmp: TempDir,
        state: Arc<Daemon>,
    }

    impl ReloadEnv {
        fn new(name: &str) -> Self {
            let env = Self {
                tmp: TempDir::new(&format!("reload-{name}")),
                state: test_daemon(cache::Power::Ac),
            };
            env.write_palette("p.toml", "#112233");
            env.write_config("p.toml");
            let path = env.dir().join("c.toml");
            let cfg = Config::load(&path).unwrap();
            let st = stamp(&path, cfg.daemon.palette_from.as_deref());
            env.state.set_config(&Arc::new(cfg));
            let mut meta = env.state.config_meta.lock().unwrap();
            meta.path = path;
            meta.stamp = st;
            drop(meta);
            env
        }

        fn dir(&self) -> &Path {
            self.tmp.path()
        }

        fn write_config(&self, palette_file: &str) {
            std::fs::write(
                self.dir().join("c.toml"),
                format!(
                    "[daemon]\npalette_from = '{}'\n[badge.a]\ntype = \"builtin\"\nname = \"hostname\"\n\
                     [groups.r]\nformat = '[{{a}}](brand)'\n",
                    self.dir().join(palette_file).display()
                ),
            )
            .unwrap();
        }

        fn write_palette(&self, file: &str, color: &str) {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            std::fs::write(
                self.dir().join(file),
                // Unique size per write: ctime granularity can be coarser than the test.
                format!(
                    "palette = \"p\"\n[palettes.p]\nbrand = \"{color}\"\n#{}\n",
                    "x".repeat(NEXT.fetch_add(1, Ordering::Relaxed))
                ),
            )
            .unwrap();
        }

        fn cfg(&self) -> Arc<Config> {
            self.state.config()
        }

        fn rendered(&self) -> String {
            let cfg = self.cfg();
            let t = cfg.groups["r"].template.as_ref().unwrap();
            t.render(Dialect::Tmux, |_| "x".into())
        }

        /// Non-forced check, each one a "later" instant than the last (never throttled).
        async fn check(&self) {
            static N: AtomicU64 = AtomicU64::new(1);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let now = Instant::now() + Duration::from_secs(3600 * n);
            check_reload(&self.state, ReloadCheck::Throttled, now).await;
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
        check_reload(&env.state, ReloadCheck::Throttled, last).await;
        assert!(Arc::ptr_eq(&before, &env.cfg()));
        // Force bypasses the throttle (and the unchanged stamp).
        check_reload(&env.state, ReloadCheck::Forced, last).await;
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
        std::fs::write(env.dir().join("p.toml"), "not [[[ toml").unwrap();
        env.check().await;
        assert!(Arc::ptr_eq(&before, &env.cfg()));
        assert_eq!(env.state.config_meta.lock().unwrap().stamp, old_stamp);
        assert!(!env.state.config_meta.lock().unwrap().checking);
        let err = env.state.config_meta.lock().unwrap().last_error.clone();
        assert!(err.unwrap().contains("reload failed"));
        // Still broken: retried (stamp not advanced), still old config.
        env.check().await;
        assert!(Arc::ptr_eq(&before, &env.cfg()));
        // Fixed: the retry succeeds.
        env.write_palette("p.toml", "#010203");
        env.check().await;
        assert!(!Arc::ptr_eq(&before, &env.cfg()));
        assert_eq!(env.rendered(), "#[fg=#010203]x#[default]");
        assert_eq!(env.state.config_meta.lock().unwrap().last_error, None);
    }

    #[tokio::test]
    async fn unchanged_config_clears_transient_error() {
        let env = ReloadEnv::new("transient");
        env.state.config_meta.lock().unwrap().last_error = Some("timed out".into());
        env.check().await;
        assert_eq!(env.state.config_meta.lock().unwrap().last_error, None);
    }

    #[tokio::test]
    async fn check_reload_palette_path_change_tracks_new_file() {
        let env = ReloadEnv::new("switch");
        env.write_palette("p2.toml", "#0000ff");
        env.write_config("p2.toml");
        env.check().await;
        assert_eq!(env.rendered(), "#[fg=#0000ff]x#[default]");
        let st = env.state.config_meta.lock().unwrap().stamp;
        assert_eq!(st.palette, file_id(&env.dir().join("p2.toml")));
        assert!(st.palette.is_some());
        // The new file is now the watched one; the old one is ignored.
        let cur = env.cfg();
        env.write_palette("p.toml", "#ff0000");
        env.check().await;
        assert!(Arc::ptr_eq(&cur, &env.cfg()));
        env.write_palette("p2.toml", "#00ff00");
        env.check().await;
        assert_eq!(env.rendered(), "#[fg=#00ff00]x#[default]");
    }

    #[tokio::test]
    async fn reload_drops_scoped_entries_of_a_changed_badge_only() {
        let tmp = TempDir::new("reload-scoped");
        let path = tmp.path().join("c.toml");
        let write = |markers: &str, env: &str| {
            let badge = |n: &str| {
                format!(
                    "[badge.{n}]\ntype = \"command\"\ncommand = \"true\"\nscope = \"project_root\"\n\
                     markers = [\"{markers}\"]\nenv = [\"{env}\"]\n"
                )
            };
            // `same` never changes; `moved` follows the arguments.
            let same = badge("same").replace(markers, "m").replace(env, "E");
            std::fs::write(&path, format!("{same}{}", badge("moved"))).unwrap();
        };
        write("m", "E");
        let state = test_daemon(cache::Power::Ac);
        state.set_config(&Arc::new(Config::load(&path).unwrap()));
        state.config_meta.lock().unwrap().path = path.clone();
        let root = Some(PathBuf::from("/r"));
        let env = Some(vec![("E".to_string(), "1".to_string())]);
        for n in ["same", "moved"] {
            state.insert(
                &Key::new(n, root.clone(), env.clone()),
                Entry::new(Instant::now()),
            );
        }
        check_reload(&state, ReloadCheck::Forced, Instant::now()).await;
        assert_eq!(state.core().cache.len(), 2, "unchanged config keeps both");

        write("m2", "E2");
        check_reload(&state, ReloadCheck::Forced, Instant::now()).await;
        let core = state.core();
        assert!(
            core.cache
                .contains_key(&Key::new("same", root.clone(), env.clone()))
        );
        assert!(!core.cache.contains_key(&Key::new("moved", root, env)));
        drop(core);
    }

    #[tokio::test]
    async fn reload_keeps_in_flight_and_resets_only_changed_badges() {
        let tmp = TempDir::new("reload-inflight");
        let path = tmp.path().join("c.toml");
        let write = |interval: &str| {
            std::fs::write(
                &path,
                format!(
                    "[badge.same]\ntype = \"command\"\ncommand = \"true\"\n\
                     [badge.edited]\ntype = \"command\"\ncommand = \"true\"\n\
                     interval = \"{interval}\"\n"
                ),
            )
            .unwrap();
        };
        write("10m");
        let state = test_daemon(cache::Power::Ac);
        state.set_config(&Arc::new(Config::load(&path).unwrap()));
        state.config_meta.lock().unwrap().path = path.clone();
        let now = Instant::now();
        for n in ["same", "edited"] {
            let mut entry = Entry::new(now);
            entry.value = Some("v".into());
            entry.fetched_at = Some(now);
            entry.in_flight = true;
            state.insert(&Key::global(n), entry);
        }
        write("20m");
        check_reload(&state, ReloadCheck::Forced, Instant::now()).await;
        let core = state.core();
        let same = &core.cache[&Key::global("same")];
        assert!(same.in_flight, "unchanged badge left as is");
        assert_eq!(same.fetched_at, Some(now));
        let edited = &core.cache[&Key::global("edited")];
        assert!(edited.in_flight, "its running fetch must not be duplicated");
        assert_eq!(edited.fetched_at, None, "refreshed under the new config");
        assert_eq!(edited.value.as_deref(), Some("v"), "last good value kept");
        drop(core);
    }

    #[tokio::test]
    async fn reload_publishes_the_union_of_env_patterns() {
        let tmp = TempDir::new("reload-env-patterns");
        let path = tmp.path().join("c.toml");
        let published = tmp.path().join("env-patterns");
        let badge = |n: &str, env: &str| {
            format!(
                "[badge.{n}]\ntype = \"command\"\ncommand = \"true\"\nscope = \"git_root\"\n\
                 env = [{env}]\n"
            )
        };
        let state = test_daemon(cache::Power::Ac);
        {
            let mut meta = state.config_meta.lock().unwrap();
            meta.path = path.clone();
            meta.env_patterns = Some(published.clone());
        }
        std::fs::write(
            &path,
            format!(
                "{}{}",
                badge("a", "\"PATH\", \"MISE_*\""),
                badge("b", "\"PATH\"")
            ),
        )
        .unwrap();
        check_reload(&state, ReloadCheck::Forced, Instant::now()).await;
        let header = crate::client::ENV_PATTERNS_HEADER;
        assert_eq!(
            std::fs::read_to_string(&published).unwrap(),
            format!("{header}\nMISE_*\nPATH\n")
        );

        // A config without `env` publishes an empty list: clients send no env at all.
        std::fs::write(
            &path,
            "[badge.c]\ntype = \"builtin\"\nname = \"hostname\"\n",
        )
        .unwrap();
        check_reload(&state, ReloadCheck::Forced, Instant::now()).await;
        assert_eq!(
            std::fs::read_to_string(&published).unwrap(),
            format!("{header}\n")
        );
    }
}
