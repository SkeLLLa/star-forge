//! Cache key/entry types and the pure scheduling math (activity window, backoff,
//! coalescing, eviction). The `HashMap<Key, Entry>` itself lives in a `std::sync::Mutex`
//! owned by `daemon.rs`, never held across `.await`.
//!
//! The idea of serving prompt/statusline calls from a daemon-held cache instead of
//! recomputing them per call comes from [beachcomber](https://github.com/NavistAu/beachcomber);
//! see `THIRD_PARTY_NOTICES.md`.
//!
//! Every value here is resolved config (`config::DaemonConfig`/`config::BadgeConfig`),
//! passed in by the caller — nothing in this module hardcodes a timing constant or derives
//! one from another (no multipliers, no `max(10*interval, 5min)`-style floors).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::BadgeConfig;
use crate::provider::GitFingerprint;

/// `None` path = a global badge; `Some(root)` = a git-root-scoped badge.
pub type Key = (String, Option<PathBuf>);

#[derive(Debug)]
pub struct Entry {
    /// Identity survives in fetch tasks after eviction, preventing their completions
    /// from writing into a replacement entry at the same key.
    pub identity: Arc<()>,
    /// Last good rendered value; never cleared by a failure.
    pub value: Option<String>,
    pub fetched_at: Option<Instant>,
    pub last_access: Instant,
    pub errors: u32,
    pub next_attempt: Instant,
    pub in_flight: bool,
    /// `.git/HEAD` + `.git/index` mtimes as of the last fetch this entry *started* (set when
    /// `should_spawn` fires, see `daemon::handle_get`); `None` for badges that don't use
    /// fingerprint invalidation (everything but `git_status`/`git_counts`). A later `get`
    /// whose freshly-read fingerprint differs forces the entry stale+cold, so the fetch that
    /// picks up the repo change lands in the same prompt instead of waiting out `interval`.
    pub fingerprint: Option<GitFingerprint>,
}

impl Entry {
    pub fn new(now: Instant) -> Self {
        Self {
            identity: Arc::new(()),
            value: None,
            fetched_at: None,
            last_access: now,
            errors: 0,
            next_attempt: now,
            in_flight: false,
            fingerprint: None,
        }
    }
}

/// `min(retry_min * 2^errors, retry_max)`, before jitter. Exposed separately so scheduling
/// tests are deterministic.
pub fn backoff_base(retry_min: Duration, retry_max: Duration, errors: u32) -> Duration {
    let shift = errors.min(31); // well past enough doublings to hit any sane retry_max
    retry_min.saturating_mul(1u32 << shift).min(retry_max)
}

/// `backoff_base` ± 20% jitter. The jitter source is cheap (sub-second micros of
/// `SystemTime::now`), not a `rand` crate. Micros, not nanos: macOS wall-clock time only
/// has microsecond resolution, so its sub-microsecond digits are always zero.
pub fn backoff(retry_min: Duration, retry_max: Duration, errors: u32) -> Duration {
    let base = backoff_base(retry_min, retry_max, errors);
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_micros();
    let frac = f64::from(micros % 1000) / 1000.0; // [0, 1)
    base.mul_f64(frac.mul_add(0.4, 0.8)) // [0.8, 1.2]
}

/// A key is active while `now - last_access < active_window` (the badge's resolved
/// `active_window`, see `config::BadgeConfig`).
pub fn is_active(last_access: Instant, active_window: Duration, now: Instant) -> bool {
    now.saturating_duration_since(last_access) < active_window
}

/// `pub(crate)`: also used by `daemon::handle_get`'s request-time staleness check, which
/// must use the same AC-aware interval the timer path uses (not always `cfg.interval`).
pub(crate) const fn effective_interval(cfg: &BadgeConfig, on_ac: bool) -> Duration {
    if on_ac {
        cfg.interval
    } else {
        cfg.battery_interval
    }
}

/// The time a global key's next refresh is due (`max(fetched_at + interval, next_attempt)`),
/// or `None` if it has no value yet, is in flight, or is dormant (inactive).
fn due_at(
    key: &Key,
    entry: &Entry,
    badges: &BTreeMap<String, BadgeConfig>,
    on_ac: bool,
    now: Instant,
) -> Option<Instant> {
    if key.1.is_some() || entry.in_flight {
        return None;
    }
    let cfg = badges.get(&key.0)?;
    let interval = effective_interval(cfg, on_ac);
    if !is_active(entry.last_access, cfg.active_window, now) {
        return None;
    }
    Some(entry.fetched_at.map_or(entry.next_attempt, |t| {
        (t + interval).max(entry.next_attempt)
    }))
}

/// Earliest due time over all active global keys; feeds `sleep_until` in the daemon's
/// single `select!` loop. `None` means nothing is active (the timer arm goes dormant).
pub fn next_due(
    cache: &HashMap<Key, Entry>,
    badges: &BTreeMap<String, BadgeConfig>,
    on_ac: bool,
    now: Instant,
) -> Option<Instant> {
    cache
        .iter()
        .filter_map(|(key, entry)| due_at(key, entry, badges, on_ac, now))
        .min()
}

/// Every active global key due within `coalesce` (the daemon's resolved `[daemon].coalesce`)
/// of `now`, for a single batched refresh.
pub fn due_keys(
    cache: &HashMap<Key, Entry>,
    badges: &BTreeMap<String, BadgeConfig>,
    on_ac: bool,
    coalesce: Duration,
    now: Instant,
) -> Vec<Key> {
    cache
        .iter()
        .filter_map(|(key, entry)| {
            let due = due_at(key, entry, badges, on_ac, now)?;
            (due <= now + coalesce).then(|| key.clone())
        })
        .collect()
}

/// Drops path-scoped keys idle for `path_evict`. Global keys are never evicted. Split out
/// from `evict` so the request path can run just this half, throttled (see
/// `daemon::maybe_evict_idle`), without repeating the LRU cap scan done on every insert.
pub fn evict_idle(cache: &mut HashMap<Key, Entry>, path_evict: Duration, now: Instant) {
    cache.retain(|key, entry| {
        key.1.is_none() || now.saturating_duration_since(entry.last_access) < path_evict
    });
}

/// LRU-evicts path-scoped keys by `last_access` down to the `max_paths` cap. Global keys
/// are never evicted. Cheap enough to call on every new path-scoped key insert
/// (`handle_get`), so the cap holds immediately instead of only after the next timer tick.
pub fn enforce_path_scoped_cap(cache: &mut HashMap<Key, Entry>, max_paths: usize) {
    let mut path_scoped: Vec<(Key, Instant)> = cache
        .iter()
        .filter(|(key, _)| key.1.is_some())
        .map(|(k, e)| (k.clone(), e.last_access))
        .collect();
    if path_scoped.len() <= max_paths {
        return;
    }
    path_scoped.sort_by_key(|(_, last_access)| *last_access);
    let excess = path_scoped.len() - max_paths;
    for (key, _) in path_scoped.into_iter().take(excess) {
        cache.remove(&key);
    }
}

/// Drops idle path-scoped keys, then enforces the LRU cap. Used by the timer path
/// (`daemon::refresh_due`), which can afford both in one pass.
pub fn evict(
    cache: &mut HashMap<Key, Entry>,
    path_evict: Duration,
    max_paths: usize,
    now: Instant,
) {
    evict_idle(cache, path_evict, now);
    enforce_path_scoped_cap(cache, max_paths);
}

#[cfg(test)]
// Test data builds `Instant`s a fixed, small offset before `Instant::now()`; that can
// never underflow, so the checked-subtraction ceremony would be pure noise here.
#[allow(clippy::unchecked_time_subtraction)]
mod tests {
    use super::*;
    use crate::config::Source;

    const MAX_PATH_SCOPED_KEYS: usize = 256;
    const PATH_SCOPED_IDLE_EVICT: Duration = Duration::from_secs(30 * 60);
    const RETRY_MIN: Duration = Duration::from_secs(2);
    const RETRY_MAX: Duration = Duration::from_secs(5 * 60);

    fn cfg(interval: Duration, battery_interval: Duration, active_window: Duration) -> BadgeConfig {
        BadgeConfig {
            source: Source::Command {
                command: "true".into(),
                args: None,
            },
            extract: None,
            format: "{value}".into(),
            interval,
            battery_interval,
            active_window,
            timeout: Duration::from_secs(2),
            max_output: 1024,
            scope: crate::config::Scope::Global,
        }
    }

    fn cfg_simple(interval_secs: u64) -> BadgeConfig {
        let interval = Duration::from_secs(interval_secs);
        cfg(interval, interval, Duration::from_secs(5 * 60))
    }

    #[test]
    fn backoff_is_monotonic_and_capped() {
        let mut prev = Duration::ZERO;
        for errors in 0..20 {
            let b = backoff_base(RETRY_MIN, RETRY_MAX, errors);
            assert!(b >= prev);
            assert!(b <= RETRY_MAX);
            prev = b;
        }
        assert_eq!(backoff_base(RETRY_MIN, RETRY_MAX, 0), RETRY_MIN);
        assert_eq!(backoff_base(RETRY_MIN, RETRY_MAX, 20), RETRY_MAX);
    }

    #[test]
    fn backoff_uses_configured_retry_bounds() {
        let min = Duration::from_millis(500);
        let max = Duration::from_secs(10);
        assert_eq!(backoff_base(min, max, 0), min);
        assert_eq!(backoff_base(min, max, 1), min * 2);
        assert_eq!(backoff_base(min, max, 10), max);
    }

    #[test]
    fn backoff_jitter_within_20_percent() {
        for errors in 0..8 {
            let base = backoff_base(RETRY_MIN, RETRY_MAX, errors);
            let jittered = backoff(RETRY_MIN, RETRY_MAX, errors);
            let lo = base.mul_f64(0.8);
            let hi = base.mul_f64(1.2);
            assert!(
                jittered >= lo && jittered <= hi,
                "errors={errors} base={base:?} jittered={jittered:?}"
            );
        }
    }

    #[test]
    fn is_active_uses_configured_window_not_a_derived_one() {
        let now = Instant::now();
        let window = Duration::from_secs(5 * 60);
        assert!(is_active(now - Duration::from_secs(60), window, now));
        assert!(!is_active(now - Duration::from_secs(301), window, now));
        // A short configured window (no 10x-interval floor) expires quickly.
        let short = Duration::from_secs(30);
        assert!(is_active(now - Duration::from_secs(29), short, now));
        assert!(!is_active(now - Duration::from_secs(31), short, now));
    }

    #[test]
    fn next_due_ignores_inactive_in_flight_and_path_scoped() {
        let now = Instant::now();
        let mut badges = BTreeMap::new();
        badges.insert("a".to_string(), cfg_simple(10));
        badges.insert("b".to_string(), cfg_simple(20));

        let mut cache = HashMap::new();
        let mut e_a = Entry::new(now - Duration::from_secs(100));
        e_a.fetched_at = Some(now - Duration::from_secs(100));
        cache.insert(("a".to_string(), None), e_a);

        let mut e_b_inflight = Entry::new(now);
        e_b_inflight.fetched_at = Some(now);
        e_b_inflight.in_flight = true;
        cache.insert(("b".to_string(), None), e_b_inflight);

        let mut e_path = Entry::new(now);
        e_path.fetched_at = Some(now - Duration::from_secs(1000));
        cache.insert(("a".to_string(), Some(PathBuf::from("/repo"))), e_path);

        let due = next_due(&cache, &badges, true, now);
        assert_eq!(
            due,
            Some(now - Duration::from_secs(100) + Duration::from_secs(10))
        );
    }

    #[test]
    fn next_due_uses_battery_interval_off_ac_not_a_multiplier() {
        let now = Instant::now();
        let mut badges = BTreeMap::new();
        badges.insert(
            "a".to_string(),
            cfg(
                Duration::from_secs(10),
                Duration::from_secs(45),
                Duration::from_secs(5 * 60),
            ),
        );
        let mut cache = HashMap::new();
        let mut e = Entry::new(now);
        e.fetched_at = Some(now);
        cache.insert(("a".to_string(), None), e);

        let on_ac = next_due(&cache, &badges, true, now).unwrap();
        let on_batt = next_due(&cache, &badges, false, now).unwrap();
        assert_eq!(on_ac - now, Duration::from_secs(10));
        assert_eq!(on_batt - now, Duration::from_secs(45));
    }

    #[test]
    fn cold_global_entries_retry_and_reload_invalidations_are_due() {
        let now = Instant::now();
        let badges = BTreeMap::from([("a".to_string(), cfg_simple(60))]);
        let mut entry = Entry::new(now);
        entry.errors = 1;
        entry.next_attempt = now + Duration::from_secs(2);
        let mut cache = HashMap::from([(("a".to_string(), None), entry)]);
        assert_eq!(
            next_due(&cache, &badges, true, now),
            Some(now + Duration::from_secs(2))
        );
        cache
            .get_mut(&("a".to_string(), None))
            .unwrap()
            .next_attempt = now;
        assert_eq!(next_due(&cache, &badges, true, now), Some(now));
    }

    #[test]
    fn due_keys_coalesces_close_refreshes() {
        let now = Instant::now();
        let mut badges = BTreeMap::new();
        badges.insert("a".to_string(), cfg_simple(10));
        badges.insert("b".to_string(), cfg_simple(10));
        let mut cache = HashMap::new();
        let mut e_a = Entry::new(now - Duration::from_secs(10));
        e_a.fetched_at = Some(now - Duration::from_secs(10));
        cache.insert(("a".to_string(), None), e_a);
        // b becomes due half a second after a -- within the 1s coalescing window.
        let mut e_b = Entry::new(now - Duration::from_secs(10));
        e_b.fetched_at = Some(now - Duration::from_millis(9500));
        cache.insert(("b".to_string(), None), e_b);

        let due = due_keys(&cache, &badges, true, Duration::from_secs(1), now);
        assert_eq!(due.len(), 2);
    }

    #[test]
    fn due_keys_respects_configured_coalesce_width() {
        let now = Instant::now();
        let mut badges = BTreeMap::new();
        badges.insert("a".to_string(), cfg_simple(10));
        badges.insert("b".to_string(), cfg_simple(10));
        let mut cache = HashMap::new();
        let mut e_a = Entry::new(now - Duration::from_secs(10));
        e_a.fetched_at = Some(now - Duration::from_secs(10));
        cache.insert(("a".to_string(), None), e_a);
        let mut e_b = Entry::new(now - Duration::from_secs(10));
        e_b.fetched_at = Some(now - Duration::from_millis(9500));
        cache.insert(("b".to_string(), None), e_b);

        // A tighter-than-default coalesce window (500ms) no longer catches the 500ms gap.
        let due = due_keys(&cache, &badges, true, Duration::from_millis(100), now);
        assert_eq!(due.len(), 1);
    }

    #[test]
    fn evict_drops_idle_path_scoped_and_caps_lru() {
        let now = Instant::now();
        let mut cache: HashMap<Key, Entry> = HashMap::new();
        cache.insert(("g".to_string(), None), Entry::new(now));
        cache.insert(
            ("g".to_string(), Some(PathBuf::from("/stale"))),
            Entry::new(now - Duration::from_secs(31 * 60)),
        );
        for i in 0..300 {
            cache.insert(
                ("g".to_string(), Some(PathBuf::from(format!("/r{i}")))),
                Entry::new(now - Duration::from_secs(i)),
            );
        }
        evict(
            &mut cache,
            PATH_SCOPED_IDLE_EVICT,
            MAX_PATH_SCOPED_KEYS,
            now,
        );
        assert!(cache.contains_key(&("g".to_string(), None)));
        assert!(!cache.contains_key(&("g".to_string(), Some(PathBuf::from("/stale")))));
        let path_scoped = cache.keys().filter(|k| k.1.is_some()).count();
        assert_eq!(path_scoped, MAX_PATH_SCOPED_KEYS);
        // The most recently accessed (smallest i) survive.
        assert!(cache.contains_key(&("g".to_string(), Some(PathBuf::from("/r0")))));
    }

    /// Mirrors what `handle_get` does on every new path-scoped key insert (design.md §4):
    /// the `max_paths` cap must hold right away, not only after the next timer tick, so a
    /// config with only path-scoped (e.g. git) badges can't overshoot it for `idle_exit`.
    #[test]
    fn enforce_path_scoped_cap_evicts_lru_immediately() {
        let now = Instant::now();
        let mut cache: HashMap<Key, Entry> = HashMap::new();
        cache.insert(("g".to_string(), None), Entry::new(now));
        for i in 0..MAX_PATH_SCOPED_KEYS {
            cache.insert(
                ("g".to_string(), Some(PathBuf::from(format!("/r{i}")))),
                Entry::new(now - Duration::from_secs(i as u64)),
            );
        }
        assert_eq!(cache.len(), MAX_PATH_SCOPED_KEYS + 1);

        // One more path-scoped key inserted (simulating `handle_get`'s
        // `cache.entry(key).or_insert_with(...)`), over the cap by one.
        cache.insert(
            ("g".to_string(), Some(PathBuf::from("/new"))),
            Entry::new(now),
        );
        enforce_path_scoped_cap(&mut cache, MAX_PATH_SCOPED_KEYS);

        let path_scoped = cache.keys().filter(|k| k.1.is_some()).count();
        assert_eq!(path_scoped, MAX_PATH_SCOPED_KEYS);
        // The global key and the just-inserted (freshest) path-scoped key both survive;
        // the single least-recently-accessed one (largest i, oldest last_access) is gone.
        assert!(cache.contains_key(&("g".to_string(), None)));
        assert!(cache.contains_key(&("g".to_string(), Some(PathBuf::from("/new")))));
        assert!(!cache.contains_key(&(
            "g".to_string(),
            Some(PathBuf::from(format!("/r{}", MAX_PATH_SCOPED_KEYS - 1)))
        )));
    }

    #[test]
    fn enforce_path_scoped_cap_respects_configured_max() {
        let now = Instant::now();
        let mut cache: HashMap<Key, Entry> = HashMap::new();
        for i in 0..10u64 {
            cache.insert(
                ("g".to_string(), Some(PathBuf::from(format!("/r{i}")))),
                Entry::new(now - Duration::from_secs(i)),
            );
        }
        enforce_path_scoped_cap(&mut cache, 3);
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn evict_idle_leaves_global_and_active_path_scoped_alone() {
        let now = Instant::now();
        let mut cache: HashMap<Key, Entry> = HashMap::new();
        cache.insert(("g".to_string(), None), Entry::new(now));
        cache.insert(
            ("g".to_string(), Some(PathBuf::from("/fresh"))),
            Entry::new(now),
        );
        cache.insert(
            ("g".to_string(), Some(PathBuf::from("/stale"))),
            Entry::new(now - Duration::from_secs(31 * 60)),
        );
        evict_idle(&mut cache, PATH_SCOPED_IDLE_EVICT, now);
        assert!(cache.contains_key(&("g".to_string(), None)));
        assert!(cache.contains_key(&("g".to_string(), Some(PathBuf::from("/fresh")))));
        assert!(!cache.contains_key(&("g".to_string(), Some(PathBuf::from("/stale")))));
    }

    #[test]
    fn evict_idle_respects_configured_evict_duration() {
        let now = Instant::now();
        let mut cache: HashMap<Key, Entry> = HashMap::new();
        cache.insert(
            ("g".to_string(), Some(PathBuf::from("/r"))),
            Entry::new(now - Duration::from_secs(5)),
        );
        // A tight path_evict (1s) drops a key idle for 5s, unlike the 30-min default.
        evict_idle(&mut cache, Duration::from_secs(1), now);
        assert!(cache.is_empty());
    }
}
