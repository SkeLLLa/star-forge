//! Config hot-reload: file identity stamps and the throttled reload check.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use super::{Daemon, MAINTENANCE_BUDGET, set_config_error};
use crate::config::{Config, Scope};

/// Identity of a file (following symlinks): changes on rewrite, rename-on-save and retarget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FileId {
    dev: u64,
    ino: u64,
    len: u64,
    ctime: i64,
    ctime_nsec: i64,
}

/// Identities of the config file and its `palette_from` file (`None` = missing/unset).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Stamp {
    pub config: Option<FileId>,
    pub palette: Option<FileId>,
}

/// Not mtime alone: `cp -p`/`rsync -a` can preserve it, and Nix store files all share mtime 1,
/// so re-pointing a symlink (home-manager) would go unnoticed. ctime can't be set from
/// userspace, and the inode changes on rename-on-save and on a symlink retarget.
pub(super) fn file_id(p: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok()?;
    Some(FileId {
        dev: m.dev(),
        ino: m.ino(),
        len: m.len(),
        ctime: m.ctime(),
        ctime_nsec: m.ctime_nsec(),
    })
}

pub(super) fn stamp(config: &Path, palette: Option<&Path>) -> Stamp {
    Stamp {
        config: file_id(config),
        palette: palette.and_then(file_id),
    }
}

/// Stats the config file at most once every `config_check` (bypassed by an explicit
/// `reload` command) and reloads if it, or its `palette_from` file, changed (inode, size or ctime). A config that fails to parse or
/// validate is logged and ignored; the previous config keeps running.
pub(super) async fn check_reload(state: &Arc<Daemon>, force: bool, now: Instant) {
    let (config_check, palette) = {
        let cfg = state.config();
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
    match result {
        Some(Some((st, Ok(new_cfg)))) => {
            let mut meta = state.config_meta.lock().unwrap();
            meta.stamp = st;
            meta.last_error = None;
            drop(meta);
            let mut current = state.config.lock().unwrap();
            let mut cache = state.cache.lock().unwrap();
            cache.retain(|key, entry| {
                let name = &key.badge;
                let Some(badge) = new_cfg.badge.get(name) else {
                    return false;
                };
                if key.root.is_some() != (badge.scope != Scope::Global) {
                    return false;
                }
                // Scoped keys embed the old root/env: if what selects them changed, they
                // can never be hit again (or hold a value computed another way).
                if key.is_scoped()
                    && current.badge.get(name).is_none_or(|old| {
                        old.source != badge.source
                            || old.scope != badge.scope
                            || old.path != badge.path
                    })
                {
                    return false;
                }
                // Keep activity and the last good value, but refresh under the new
                // config. Stale fetch completions are rejected by `spawn_fetch`'s `Arc::ptr_eq` on the
                // config and the entry identity.
                entry.fetched_at = None;
                entry.in_flight = false;
                entry.fingerprint = None;
                entry.watch = None;
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
            let msg = format!("config reload failed, keeping previous config: {e}");
            set_config_error(&mut state.config_meta.lock().unwrap(), msg);
        }
        None => {
            let msg = "config check timed out or filesystem workers are busy".into();
            set_config_error(&mut state.config_meta.lock().unwrap(), msg);
        }
        // Unchanged stamp: a failed parse never stores it, so any old error was transient.
        Some(None) => state.config_meta.lock().unwrap().last_error = None,
    }
    state.config_meta.lock().unwrap().checking = false;
    state.reload_finished.notify_waiters();
}

/// Implicit checks must not consume the prompt's latency budget.
pub(super) fn schedule_reload_check(state: &Arc<Daemon>, now: Instant) {
    let config_check = state.config().daemon.config_check;
    let due = {
        let meta = state.config_meta.lock().unwrap();
        !meta.checking && now.saturating_duration_since(meta.last_checked) >= config_check
    };
    if due {
        let state = Arc::clone(state);
        tokio::spawn(async move { check_reload(&state, false, now).await });
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

    /// Temp dir with a config (`c.toml`, group `r` = `[{a}](brand)`) and palette (`p.toml`).
    struct ReloadEnv {
        tmp: TempDir,
        state: Arc<Daemon>,
    }

    impl ReloadEnv {
        fn new(name: &str) -> Self {
            let env = Self {
                tmp: TempDir::new(&format!("reload-{name}")),
                state: test_daemon(true),
            };
            env.write_palette("p.toml", "#112233");
            env.write_config("p.toml");
            let path = env.dir().join("c.toml");
            let cfg = Config::load(&path).unwrap();
            let st = stamp(&path, cfg.daemon.palette_from.as_deref());
            *env.state.config.lock().unwrap() = Arc::new(cfg);
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
                    "x".repeat(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
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
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let now = Instant::now() + Duration::from_secs(3600 * n);
            check_reload(&self.state, false, now).await;
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
        let state = test_daemon(true);
        *state.config.lock().unwrap() = Arc::new(Config::load(&path).unwrap());
        state.config_meta.lock().unwrap().path = path.clone();
        let root = Some(PathBuf::from("/r"));
        let env = Some(vec![("E".to_string(), "1".to_string())]);
        for n in ["same", "moved"] {
            state.cache.lock().unwrap().insert(
                cache::Key::new(n, root.clone(), env.clone()),
                cache::Entry::new(Instant::now()),
            );
        }
        check_reload(&state, true, Instant::now()).await;
        assert_eq!(
            state.cache.lock().unwrap().len(),
            2,
            "unchanged config keeps both"
        );

        write("m2", "E2");
        check_reload(&state, true, Instant::now()).await;
        let cache = state.cache.lock().unwrap();
        assert!(cache.contains_key(&cache::Key::new("same", root.clone(), env.clone())));
        assert!(!cache.contains_key(&cache::Key::new("moved", root, env)));
        drop(cache);
    }
}
