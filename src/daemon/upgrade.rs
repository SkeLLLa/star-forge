//! Upgrade safety (design.md §1): when a `get` from another client version should make this
//! daemon exit so the next prompt spawns the new binary.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::file_id::{FileId, file_id};

/// Least uptime before a newer client may restart the daemon when its own binary is
/// unchanged: if the client can't find a newer `stfgd` to spawn, the replacement is this
/// same version again, and this bounds that loop to one restart per interval.
pub(super) const MIN_UPTIME_FOR_NEWER_CLIENT: Duration = Duration::from_secs(60);

pub(super) struct Upgrade {
    /// The daemon's executable as it was at startup: `current_exe()` and its identity. The
    /// path is kept because Linux's `/proc/self/exe` points at the old, deleted inode after
    /// an upgrade replaces the file.
    exe: Option<(PathBuf, FileId)>,
    started: Instant,
    /// Last client version that differed from ours; shown by `stfgd status`.
    last_mismatch: Mutex<Option<String>>,
}

/// Whether the executable on disk is still the one this daemon runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Exe {
    Same,
    Replaced,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Restart,
    Stay,
}

impl Upgrade {
    /// Records the running executable. Call before the runtime starts (a `stat`).
    pub(super) fn record(started: Instant) -> Self {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| file_id(&p).map(|id| (p, id)));
        Self::new(exe, started)
    }

    pub(super) const fn new(exe: Option<(PathBuf, FileId)>, started: Instant) -> Self {
        Self {
            exe,
            started,
            last_mismatch: Mutex::new(None),
        }
    }

    /// A job for the blocking pool that checks the executable on disk (`Same` if it couldn't
    /// be recorded at startup).
    pub(super) fn exe_check_job(&self) -> impl FnOnce() -> Exe + Send + 'static {
        let exe = self.exe.clone();
        move || {
            if exe.is_some_and(|(path, id)| file_id(&path) != Some(id)) {
                Exe::Replaced
            } else {
                Exe::Same
            }
        }
    }

    /// Notes a mismatched client version (for `status`).
    pub(super) fn note_mismatch(&self, client: &str) {
        *self
            .last_mismatch
            .lock()
            .expect("last_mismatch lock poisoned") = Some(client.to_string());
    }

    pub(super) fn last_mismatch(&self) -> Option<String> {
        self.last_mismatch
            .lock()
            .expect("last_mismatch lock poisoned")
            .clone()
    }

    /// Restart when our binary was replaced (that's the upgrade), or when the client is newer
    /// and we've been up long enough. An older client never restarts us: it would only spawn
    /// this same binary again, on every prompt.
    pub(super) fn decide(&self, ours: &str, client: &str, exe: Exe, now: Instant) -> Verdict {
        if exe == Exe::Replaced
            || (is_newer(client, ours)
                && now.saturating_duration_since(self.started) >= MIN_UPTIME_FOR_NEWER_CLIENT)
        {
            Verdict::Restart
        } else {
            Verdict::Stay
        }
    }
}

/// `major.minor.patch`, ignoring any `-pre`/`+build` suffix.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(str::parse::<u64>);
    let version = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    parts.next().is_none().then_some(version)
}

/// Whether `a` is a strictly newer release than `b`; unparsable versions are never newer.
fn is_newer(a: &str, b: &str) -> bool {
    matches!((parse_version(a), parse_version(b)), (Some(a), Some(b)) if a > b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_and_ignore_suffixes() {
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(is_newer("2.0.0-rc.1", "1.99.0"));
        assert!(!is_newer("1.6.0", "1.6.0+build"));
        assert!(!is_newer("0.0.0-old", "1.6.0"));
        assert!(!is_newer("garbage", "1.6.0"));
        assert!(!is_newer("1.7", "1.6.0"));
    }

    #[test]
    fn only_a_changed_binary_or_a_newer_client_after_min_uptime_restarts() {
        let start = Instant::now();
        let u = Upgrade::new(None, start);
        let early = start + Duration::from_secs(1);
        let late = start + MIN_UPTIME_FOR_NEWER_CLIENT;
        assert_eq!(u.decide("1.6.0", "1.5.0", Exe::Same, late), Verdict::Stay);
        assert_eq!(u.decide("1.6.0", "1.7.0", Exe::Same, early), Verdict::Stay);
        assert_eq!(
            u.decide("1.6.0", "1.7.0", Exe::Same, late),
            Verdict::Restart
        );
        assert_eq!(
            u.decide("1.6.0", "1.5.0", Exe::Replaced, early),
            Verdict::Restart
        );
    }

    #[test]
    fn exe_change_is_seen_through_the_recorded_path() {
        let dir = crate::test_support::TempDir::new("upgrade-exe");
        let exe = dir.path().join("stfgd");
        std::fs::write(&exe, "old").unwrap();
        let u = Upgrade::new(Some((exe.clone(), file_id(&exe).unwrap())), Instant::now());
        assert_eq!(u.exe_check_job()(), Exe::Same);
        std::fs::write(dir.path().join("stfgd.new"), "new").unwrap();
        std::fs::rename(dir.path().join("stfgd.new"), &exe).unwrap();
        assert_eq!(u.exe_check_job()(), Exe::Replaced);
        assert_eq!(
            Upgrade::new(None, Instant::now()).exe_check_job()(),
            Exe::Same
        );
    }
}
