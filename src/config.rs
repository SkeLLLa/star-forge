//! Config types and loading. `Source`/`Extract` are closed enums, dispatched with `match`
//! (no traits, no plugin system).
//!
//! Every timing knob is an explicit config value with a single fixed default — no derived
//! formulas or multipliers. Durations are parsed once at load time (`duration::parse`) into
//! a plain struct of `Duration`s, so the scheduler never computes a fallback itself.
//! Resolution order for a per-badge knob is: badge value → `[daemon]` value → fixed
//! default, except `battery_interval`, which has its own four-level chain (see
//! `resolve_battery_interval`). A config that fails to parse or validate is rejected
//! wholesale; the caller (`daemon::check_reload`) keeps the previous config running.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::duration;
use crate::extract::{Extract, RawExtract};

/// Builtin providers, keyed by name in `[badge.*] type = "builtin"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Builtin {
    GitBranch,
    GitStatus,
    /// One of `GitField`'s counts, read via one `git status --porcelain=v2 --branch` call.
    GitCounts,
    /// Short commit hash, resolved in-process from `HEAD` (no subprocess).
    GitCommit,
    /// `REBASING`/`MERGING`/`CHERRY-PICKING`/`BISECTING`, read from `.git` state files.
    GitState,
    /// Stash entry count, read from the stash reflog.
    GitStash,
    Battery,
    Hostname,
    LoadAvg,
    MemUsedPercent,
    Uptime,
}

impl Builtin {
    /// Git builtins are keyed by repo root; everything else is a global key.
    pub const fn is_path_scoped(self) -> bool {
        matches!(
            self,
            Self::GitBranch
                | Self::GitStatus
                | Self::GitCounts
                | Self::GitCommit
                | Self::GitState
                | Self::GitStash
        )
    }
}

/// Which field `name = "git_counts"` reports; required for `git_counts`, rejected on every
/// other builtin (see `RawBadgeConfig::into_parts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitField {
    Ahead,
    Behind,
    Staged,
    Modified,
    Untracked,
    Conflicted,
}

/// Cache scope for a badge: git builtins always use `GitRoot`; everything else defaults to
/// `Global`. Non-git builtins, `command`, and `http` badges can override either scope via
/// `scope = "git_root" | "global"` — e.g. a command badge scoped to `git_root` runs with cwd
/// = the repo root and is cached per repo, same as a git builtin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Global,
    GitRoot,
}

/// Where a badge's raw value comes from (compiled: `device` already validated against
/// `name`, see `RawBadgeConfig::into_parts`).
#[derive(Debug, Clone)]
pub enum Source {
    Builtin {
        name: Builtin,
        /// `battery` only. Linux: which `/sys/class/power_supply/*` entry to read; `None`
        /// picks the first whose `type` is `Battery`. macOS: a power-source name exactly as
        /// `pmset -g batt` prints it (e.g. `InternalBattery-0`, or a UPS); `None` picks the
        /// first `InternalBattery*`.
        device: Option<String>,
        /// `git_counts` only: which count to report.
        field: Option<GitField>,
    },
    Command {
        command: String,
        /// `None` -> run `sh -c <command>` (pipelines, `&&`, …); `Some` -> exec `command`
        /// directly with these args, no shell.
        args: Option<Vec<String>>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

fn default_format() -> String {
    "{value}".to_string()
}

/// One `[badge.<name>]` entry, deserialized directly (no `#[serde(flatten)]`): each variant
/// duplicates the handful of common fields so `deny_unknown_fields` actually catches config
/// typos (flatten + `deny_unknown_fields` don't combine in serde — see design.md §4).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RawBadgeConfig {
    Builtin {
        name: Builtin,
        /// `battery`-only option; rejected on every other builtin (`into_parts`).
        #[serde(default)]
        device: Option<String>,
        /// `git_counts`-only option; required for it, rejected on every other builtin
        /// (`into_parts`).
        #[serde(default)]
        field: Option<GitField>,
        #[serde(default = "default_format")]
        format: String,
        #[serde(default)]
        interval: Option<String>,
        #[serde(default)]
        battery_interval: Option<String>,
        #[serde(default)]
        active_window: Option<String>,
        #[serde(default)]
        timeout: Option<String>,
        #[serde(default)]
        max_output: Option<usize>,
        #[serde(default)]
        extract: Option<RawExtract>,
        #[serde(default)]
        scope: Option<Scope>,
    },
    Command {
        command: String,
        #[serde(default)]
        args: Option<Vec<String>>,
        #[serde(default = "default_format")]
        format: String,
        #[serde(default)]
        interval: Option<String>,
        #[serde(default)]
        battery_interval: Option<String>,
        #[serde(default)]
        active_window: Option<String>,
        #[serde(default)]
        timeout: Option<String>,
        #[serde(default)]
        max_output: Option<usize>,
        #[serde(default)]
        extract: Option<RawExtract>,
        #[serde(default)]
        scope: Option<Scope>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default = "default_format")]
        format: String,
        #[serde(default)]
        interval: Option<String>,
        #[serde(default)]
        battery_interval: Option<String>,
        #[serde(default)]
        active_window: Option<String>,
        #[serde(default)]
        timeout: Option<String>,
        #[serde(default)]
        max_output: Option<usize>,
        #[serde(default)]
        extract: Option<RawExtract>,
        #[serde(default)]
        scope: Option<Scope>,
    },
}

/// Fields shared by every badge type, pulled out of whichever `RawBadgeConfig` variant
/// matched (see `RawBadgeConfig::into_parts`).
struct RawCommon {
    format: String,
    interval: Option<String>,
    battery_interval: Option<String>,
    active_window: Option<String>,
    timeout: Option<String>,
    max_output: Option<usize>,
    extract: Option<RawExtract>,
    scope: Option<Scope>,
}

impl RawBadgeConfig {
    /// Splits into the provider-specific `Source` and the shared timing/format fields.
    /// Validates builtin-specific options against the wrong builtin (e.g. `device` on
    /// `git_branch`), since serde can't express that conditionally.
    fn into_parts(self, badge_name: &str) -> Result<(Source, RawCommon), String> {
        match self {
            Self::Builtin {
                name,
                device,
                field,
                format,
                interval,
                battery_interval,
                active_window,
                timeout,
                max_output,
                extract,
                scope,
            } => {
                if device.is_some() && name != Builtin::Battery {
                    return Err(format!(
                        "badge {badge_name}: `device` is only valid for `type = \"builtin\"` \
                         `name = \"battery\"`"
                    ));
                }
                if field.is_some() && name != Builtin::GitCounts {
                    return Err(format!(
                        "badge {badge_name}: `field` is only valid for `type = \"builtin\"` \
                         `name = \"git_counts\"`"
                    ));
                }
                if field.is_none() && name == Builtin::GitCounts {
                    return Err(format!(
                        "badge {badge_name}: `type = \"builtin\"` `name = \"git_counts\"` \
                         requires `field`"
                    ));
                }
                Ok((
                    Source::Builtin {
                        name,
                        device,
                        field,
                    },
                    RawCommon {
                        format,
                        interval,
                        battery_interval,
                        active_window,
                        timeout,
                        max_output,
                        extract,
                        scope,
                    },
                ))
            }
            Self::Command {
                command,
                args,
                format,
                interval,
                battery_interval,
                active_window,
                timeout,
                max_output,
                extract,
                scope,
            } => Ok((
                Source::Command { command, args },
                RawCommon {
                    format,
                    interval,
                    battery_interval,
                    active_window,
                    timeout,
                    max_output,
                    extract,
                    scope,
                },
            )),
            Self::Http {
                url,
                headers,
                format,
                interval,
                battery_interval,
                active_window,
                timeout,
                max_output,
                extract,
                scope,
            } => Ok((
                Source::Http { url, headers },
                RawCommon {
                    format,
                    interval,
                    battery_interval,
                    active_window,
                    timeout,
                    max_output,
                    extract,
                    scope,
                },
            )),
        }
    }
}

/// One `[badge.<name>]` entry, ready to use (extract regex already compiled, every timing
/// knob resolved to a concrete `Duration`).
#[derive(Debug)]
pub struct BadgeConfig {
    pub source: Source,
    pub extract: Option<Extract>,
    pub format: String,
    pub interval: Duration,
    pub battery_interval: Duration,
    pub active_window: Duration,
    pub timeout: Duration,
    pub max_output: usize,
    pub scope: Scope,
}

/// Default cache scope when a badge doesn't set `scope` explicitly: the path-scoped
/// builtins (the git ones) default to `git_root`, everything else to `global`.
const fn default_scope(source: &Source) -> Scope {
    match source {
        Source::Builtin { name, .. } if name.is_path_scoped() => Scope::GitRoot,
        _ => Scope::Global,
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RawDaemonConfig {
    interval: Option<String>,
    battery_interval: Option<String>,
    active_window: Option<String>,
    coalesce: Option<String>,
    retry_min: Option<String>,
    retry_max: Option<String>,
    timeout: Option<String>,
    power_check: Option<String>,
    config_check: Option<String>,
    cold_wait: Option<String>,
    idle_exit: Option<String>,
    path_evict: Option<String>,
    max_paths: Option<usize>,
    max_output: Option<usize>,
}

const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_ACTIVE_WINDOW: Duration = Duration::from_secs(5 * 60);
const DEFAULT_COALESCE: Duration = Duration::from_secs(1);
const DEFAULT_RETRY_MIN: Duration = Duration::from_secs(2);
const DEFAULT_RETRY_MAX: Duration = Duration::from_secs(5 * 60);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_POWER_CHECK: Duration = Duration::from_secs(60);
const DEFAULT_CONFIG_CHECK: Duration = Duration::from_secs(2);
const DEFAULT_COLD_WAIT: Duration = Duration::from_millis(20);
const DEFAULT_IDLE_EXIT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_PATH_EVICT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_MAX_PATHS: usize = 256;
const DEFAULT_MAX_OUTPUT: usize = 64 * 1024;
/// Minimum client deadline (see `client.rs`); `cold_wait` must stay under this.
const CLIENT_DEADLINE: Duration = Duration::from_millis(40);

/// `[daemon]`, fully resolved: every field is a concrete value, defaulted per the schema in
/// design.md §7 (no derived formulas — `battery_interval`'s only fallback is `interval`
/// itself, see `Config::load`).
#[derive(Debug)]
pub struct DaemonConfig {
    pub interval: Duration,
    pub battery_interval: Duration,
    pub active_window: Duration,
    pub coalesce: Duration,
    pub retry_min: Duration,
    pub retry_max: Duration,
    pub timeout: Duration,
    pub power_check: Duration,
    pub config_check: Duration,
    pub cold_wait: Duration,
    pub idle_exit: Duration,
    pub path_evict: Duration,
    pub max_paths: usize,
    pub max_output: usize,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            battery_interval: DEFAULT_INTERVAL, // unset = same as interval
            active_window: DEFAULT_ACTIVE_WINDOW,
            coalesce: DEFAULT_COALESCE,
            retry_min: DEFAULT_RETRY_MIN,
            retry_max: DEFAULT_RETRY_MAX,
            timeout: DEFAULT_TIMEOUT,
            power_check: DEFAULT_POWER_CHECK,
            config_check: DEFAULT_CONFIG_CHECK,
            cold_wait: DEFAULT_COLD_WAIT,
            idle_exit: DEFAULT_IDLE_EXIT,
            path_evict: DEFAULT_PATH_EVICT,
            max_paths: DEFAULT_MAX_PATHS,
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    daemon: RawDaemonConfig,
    #[serde(default)]
    badge: BTreeMap<String, RawBadgeConfig>,
}

#[derive(Debug)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub badge: BTreeMap<String, BadgeConfig>,
}

/// Parses `raw` (if set) or falls back to `default`, tagging any parse error with `ctx`.
fn resolve(raw: Option<&str>, default: Duration, ctx: &str) -> Result<Duration, String> {
    raw.map_or(Ok(default), |s| {
        duration::parse(s).map_err(|e| format!("{ctx}: {e}"))
    })
}

impl Config {
    /// Parses, compiles, and validates a config file. A missing file is treated as an empty
    /// config (no badges, default daemon settings) so a first run works without setup.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        let raw: RawConfig =
            toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;

        let interval = resolve(
            raw.daemon.interval.as_deref(),
            DEFAULT_INTERVAL,
            "daemon.interval",
        )?;
        // No hardcoded 120s here: an unset daemon.battery_interval is simply the same as
        // daemon.interval (design.md §4/§7) -- the only fallback chain in this file that
        // isn't a fixed constant.
        let battery_interval = resolve(
            raw.daemon.battery_interval.as_deref(),
            interval,
            "daemon.battery_interval",
        )?;
        let daemon = DaemonConfig {
            interval,
            battery_interval,
            active_window: resolve(
                raw.daemon.active_window.as_deref(),
                DEFAULT_ACTIVE_WINDOW,
                "daemon.active_window",
            )?,
            coalesce: resolve(
                raw.daemon.coalesce.as_deref(),
                DEFAULT_COALESCE,
                "daemon.coalesce",
            )?,
            retry_min: resolve(
                raw.daemon.retry_min.as_deref(),
                DEFAULT_RETRY_MIN,
                "daemon.retry_min",
            )?,
            retry_max: resolve(
                raw.daemon.retry_max.as_deref(),
                DEFAULT_RETRY_MAX,
                "daemon.retry_max",
            )?,
            timeout: resolve(
                raw.daemon.timeout.as_deref(),
                DEFAULT_TIMEOUT,
                "daemon.timeout",
            )?,
            power_check: resolve(
                raw.daemon.power_check.as_deref(),
                DEFAULT_POWER_CHECK,
                "daemon.power_check",
            )?,
            config_check: resolve(
                raw.daemon.config_check.as_deref(),
                DEFAULT_CONFIG_CHECK,
                "daemon.config_check",
            )?,
            cold_wait: resolve(
                raw.daemon.cold_wait.as_deref(),
                DEFAULT_COLD_WAIT,
                "daemon.cold_wait",
            )?,
            idle_exit: resolve(
                raw.daemon.idle_exit.as_deref(),
                DEFAULT_IDLE_EXIT,
                "daemon.idle_exit",
            )?,
            path_evict: resolve(
                raw.daemon.path_evict.as_deref(),
                DEFAULT_PATH_EVICT,
                "daemon.path_evict",
            )?,
            max_paths: raw.daemon.max_paths.unwrap_or(DEFAULT_MAX_PATHS),
            max_output: raw.daemon.max_output.unwrap_or(DEFAULT_MAX_OUTPUT),
        };

        let mut badge = BTreeMap::new();
        for (name, raw_badge) in raw.badge {
            let (source, common) = raw_badge.into_parts(&name)?;
            let extract = common
                .extract
                .map(RawExtract::compile)
                .transpose()
                .map_err(|e| format!("badge {name}: {e}"))?;

            let badge_interval = match common.interval.as_deref() {
                Some(s) => duration::parse(s).map_err(|e| format!("badge {name}.interval: {e}"))?,
                None => daemon.interval,
            };
            // battery_interval resolution order: badge.battery_interval -> badge.interval
            // (if set) -> daemon.battery_interval -> daemon.interval (the last two already
            // folded into `daemon.battery_interval` above).
            let badge_battery_interval = if let Some(s) = common.battery_interval.as_deref() {
                duration::parse(s).map_err(|e| format!("badge {name}.battery_interval: {e}"))?
            } else if common.interval.is_some() {
                badge_interval
            } else {
                daemon.battery_interval
            };
            let active_window = match common.active_window.as_deref() {
                Some(s) => {
                    duration::parse(s).map_err(|e| format!("badge {name}.active_window: {e}"))?
                }
                None => daemon.active_window,
            };
            let timeout = match common.timeout.as_deref() {
                Some(s) => duration::parse(s).map_err(|e| format!("badge {name}.timeout: {e}"))?,
                None => daemon.timeout,
            };
            let max_output = common.max_output.unwrap_or(daemon.max_output);
            let default_scope = default_scope(&source);
            let scope = match (common.scope, default_scope) {
                (Some(Scope::Global), Scope::GitRoot) => {
                    return Err(format!(
                        "badge {name}: git builtins require `scope = \"git_root\"`; \
                         `scope = \"global\"` is invalid here"
                    ));
                }
                (Some(scope), _) => scope,
                (None, default_scope) => default_scope,
            };

            badge.insert(
                name,
                BadgeConfig {
                    source,
                    extract,
                    format: common.format,
                    interval: badge_interval,
                    battery_interval: badge_battery_interval,
                    active_window,
                    timeout,
                    max_output,
                    scope,
                },
            );
        }

        validate(&daemon, &badge)?;
        Ok(Self { daemon, badge })
    }
}

/// Rejects a config whose resolved values would make the scheduler misbehave: this runs
/// after every value is resolved, so it sees the same numbers the scheduler will use.
fn validate(daemon: &DaemonConfig, badge: &BTreeMap<String, BadgeConfig>) -> Result<(), String> {
    let one_sec = Duration::from_secs(1);
    if daemon.interval < one_sec {
        return Err("daemon.interval must be >= 1s".to_string());
    }
    if daemon.battery_interval < one_sec {
        return Err("daemon.battery_interval must be >= 1s".to_string());
    }
    if daemon.retry_min > daemon.retry_max {
        return Err("daemon.retry_min must be <= daemon.retry_max".to_string());
    }
    if daemon.cold_wait >= CLIENT_DEADLINE {
        return Err(format!(
            "daemon.cold_wait must be < {}ms (the client deadline)",
            CLIENT_DEADLINE.as_millis()
        ));
    }
    if daemon.timeout == Duration::ZERO {
        return Err("daemon.timeout must be > 0".to_string());
    }
    if daemon.max_paths < 1 {
        return Err("daemon.max_paths must be >= 1".to_string());
    }

    // coalesce must stay smaller than every resolved interval, or a batch of refreshes due
    // seconds apart would collapse into the same wakeup as ones due a full interval apart.
    let mut intervals = vec![
        ("daemon.interval", daemon.interval),
        ("daemon.battery_interval", daemon.battery_interval),
    ];
    for (name, cfg) in badge {
        if cfg.interval < one_sec {
            return Err(format!("badge {name}.interval must be >= 1s"));
        }
        if cfg.battery_interval < one_sec {
            return Err(format!("badge {name}.battery_interval must be >= 1s"));
        }
        if cfg.timeout == Duration::ZERO {
            return Err(format!("badge {name}.timeout must be > 0"));
        }
        intervals.push((name.as_str(), cfg.interval));
        intervals.push((name.as_str(), cfg.battery_interval));
    }
    for (ctx, interval) in intervals {
        if daemon.coalesce >= interval {
            return Err(format!(
                "daemon.coalesce ({:?}) must be < every resolved interval, but {ctx} resolves to {:?}",
                daemon.coalesce, interval
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write_temp(contents: &str) -> tempfile_path::TempPath {
        tempfile_path::write(contents)
    }

    /// Minimal same-crate stand-in for a temp file, so we don't add a dev-dependency.
    mod tempfile_path {
        use super::*;
        use std::path::PathBuf;

        pub struct TempPath(pub PathBuf);
        impl std::ops::Deref for TempPath {
            type Target = Path;
            fn deref(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for TempPath {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        pub fn write(contents: &str) -> TempPath {
            // macOS clocks tick in microseconds, so parallel tests can read the same
            // timestamp; the counter keeps their paths distinct.
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "star-forge-test-{}-{}-{}.toml",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(contents.as_bytes()).unwrap();
            TempPath(path)
        }
    }

    #[test]
    fn missing_file_is_empty_config() {
        let cfg = Config::load(Path::new("/nonexistent/star-forge-config.toml")).unwrap();
        assert!(cfg.badge.is_empty());
        assert_eq!(cfg.daemon.idle_exit, Duration::from_secs(30 * 60));
        assert_eq!(cfg.daemon.interval, Duration::from_secs(60));
        assert_eq!(cfg.daemon.battery_interval, Duration::from_secs(60));
    }

    #[test]
    fn parses_builtin_command_http_badges() {
        let toml = r#"
[daemon]
idle_exit = "60s"

[badge.git_branch]
type = "builtin"
name = "git_branch"
interval = "3s"

[badge.kernel]
type = "command"
command = "uname"
args = ["-r"]
interval = "1h"
extract = { kind = "regex", pattern = '^(\d+\.\d+)', group = 1 }

[badge.public_ip]
type = "http"
url = "https://api.ipify.org?format=json"
interval = "10m"
extract = { kind = "json", pointer = "/ip" }
"#;
        let path = write_temp(toml);
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.daemon.idle_exit, Duration::from_secs(60));
        assert_eq!(cfg.badge.len(), 3);
        match &cfg.badge["git_branch"].source {
            Source::Builtin { name, device, .. } => {
                assert_eq!(*name, Builtin::GitBranch);
                assert!(device.is_none());
            }
            _ => panic!("wrong source"),
        }
        match &cfg.badge["kernel"].source {
            Source::Command { command, args } => {
                assert_eq!(command, "uname");
                assert_eq!(args, &Some(vec!["-r".to_string()]));
            }
            _ => panic!("wrong source"),
        }
        assert!(matches!(
            cfg.badge["kernel"].extract,
            Some(Extract::Regex { .. })
        ));
        assert!(matches!(
            cfg.badge["public_ip"].extract,
            Some(Extract::Json { .. })
        ));
    }

    #[test]
    fn invalid_toml_errors() {
        let path = write_temp("not valid [[[ toml");
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn invalid_regex_extract_errors() {
        let toml = r#"
[badge.bad]
type = "command"
command = "echo"
interval = "1s"
extract = { kind = "regex", pattern = "(", group = 0 }
"#;
        let path = write_temp(toml);
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn defaults_are_applied() {
        let toml = r#"
[badge.hostname]
type = "builtin"
name = "hostname"
interval = "5s"
"#;
        let path = write_temp(toml);
        let cfg = Config::load(&path).unwrap();
        let b = &cfg.badge["hostname"];
        assert_eq!(b.format, "{value}");
        assert_eq!(b.timeout, Duration::from_secs(2));
        assert_eq!(b.max_output, 64 * 1024);
    }

    #[test]
    fn device_on_non_battery_builtin_is_rejected() {
        let toml = r#"
[badge.branch]
type = "builtin"
name = "git_branch"
device = "BAT0"
"#;
        let path = write_temp(toml);
        let err = Config::load(&path).unwrap_err();
        assert!(err.contains("device"), "error was: {err}");
    }

    #[test]
    fn device_on_battery_builtin_is_accepted() {
        let toml = r#"
[badge.battery]
type = "builtin"
name = "battery"
device = "BAT0"
"#;
        let path = write_temp(toml);
        let cfg = Config::load(&path).unwrap();
        match &cfg.badge["battery"].source {
            Source::Builtin { name, device, .. } => {
                assert_eq!(*name, Builtin::Battery);
                assert_eq!(device.as_deref(), Some("BAT0"));
            }
            _ => panic!("wrong source"),
        }
    }

    #[test]
    fn field_on_non_git_counts_builtin_is_rejected() {
        let toml = r#"
[badge.branch]
type = "builtin"
name = "git_branch"
field = "ahead"
"#;
        let path = write_temp(toml);
        let err = Config::load(&path).unwrap_err();
        assert!(err.contains("field"), "error was: {err}");
    }

    #[test]
    fn git_counts_without_field_is_rejected() {
        let toml = r#"
[badge.ahead]
type = "builtin"
name = "git_counts"
"#;
        let path = write_temp(toml);
        let err = Config::load(&path).unwrap_err();
        assert!(err.contains("field"), "error was: {err}");
    }

    #[test]
    fn git_counts_with_field_is_accepted() {
        let toml = r#"
[badge.ahead]
type = "builtin"
name = "git_counts"
field = "ahead"
"#;
        let path = write_temp(toml);
        let cfg = Config::load(&path).unwrap();
        match &cfg.badge["ahead"].source {
            Source::Builtin { name, field, .. } => {
                assert_eq!(*name, Builtin::GitCounts);
                assert_eq!(*field, Some(GitField::Ahead));
            }
            _ => panic!("wrong source"),
        }
    }

    #[test]
    fn unknown_field_is_rejected() {
        let toml = r#"
[badge.oops]
type = "command"
command = "echo"
bogus_field = "x"
"#;
        let path = write_temp(toml);
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn unknown_daemon_field_is_rejected() {
        let toml = r#"
[daemon]
bogus_field = "x"
"#;
        let path = write_temp(toml);
        assert!(Config::load(&path).is_err());
    }

    /// The four documented fallback levels for a badge's `battery_interval` (design.md §7).
    #[test]
    fn battery_interval_resolution_order() {
        // 1. badge.battery_interval wins outright.
        let path = write_temp(
            r#"
[badge.a]
type = "command"
command = "echo"
interval = "10s"
battery_interval = "40s"
"#,
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.badge["a"].battery_interval, Duration::from_secs(40));

        // 2. no badge.battery_interval -> falls back to badge.interval.
        let path = write_temp(
            r#"
[badge.a]
type = "command"
command = "echo"
interval = "15s"
"#,
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.badge["a"].battery_interval, Duration::from_secs(15));

        // 3. no badge interval/battery_interval -> falls back to daemon.battery_interval.
        let path = write_temp(
            r#"
[daemon]
interval = "30s"
battery_interval = "90s"

[badge.a]
type = "command"
command = "echo"
"#,
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.badge["a"].battery_interval, Duration::from_secs(90));

        // 4. nothing set anywhere -> falls back to daemon.interval (its own resolved
        // value, since daemon.battery_interval is itself unset).
        let path = write_temp(
            r#"
[daemon]
interval = "45s"

[badge.a]
type = "command"
command = "echo"
"#,
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.badge["a"].battery_interval, Duration::from_secs(45));
    }

    #[test]
    fn validation_rejects_sub_second_interval() {
        let path = write_temp(
            r#"
[daemon]
interval = "500ms"
"#,
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn validation_rejects_coalesce_not_below_interval() {
        let path = write_temp(
            r#"
[daemon]
interval = "1s"
coalesce = "1s"
"#,
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn validation_rejects_retry_min_above_retry_max() {
        let path = write_temp(
            r#"
[daemon]
retry_min = "10m"
retry_max = "5m"
"#,
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn validation_rejects_cold_wait_at_or_above_40ms() {
        let path = write_temp(
            r#"
[daemon]
cold_wait = "40ms"
"#,
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn validation_rejects_zero_max_paths() {
        let path = write_temp(
            r"
[daemon]
max_paths = 0
",
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn validation_rejects_zero_timeout() {
        let path = write_temp(
            r#"
[badge.a]
type = "command"
command = "echo"
timeout = "0s"
"#,
        );
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn scope_defaults_to_git_root_for_path_scoped_builtins_and_global_otherwise() {
        let toml = r#"
[badge.git_branch]
type = "builtin"
name = "git_branch"

[badge.hostname]
type = "builtin"
name = "hostname"
"#;
        let path = write_temp(toml);
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.badge["git_branch"].scope, Scope::GitRoot);
        assert_eq!(cfg.badge["hostname"].scope, Scope::Global);
    }

    #[test]
    fn all_git_builtins_require_git_root_scope() {
        let git_builtins = [
            ("git_branch", None),
            ("git_status", None),
            ("git_counts", Some("ahead")),
            ("git_commit", None),
            ("git_state", None),
            ("git_stash", None),
        ];
        let scopes = [
            ("default", "", true),
            ("explicit git_root", "scope = \"git_root\"", true),
            ("explicit global", "scope = \"global\"", false),
        ];

        for (builtin, field) in git_builtins {
            let field = field.map_or_else(String::new, |field| format!("field = \"{field}\"\n"));
            for (scope_case, scope_config, accepted) in scopes {
                let toml = format!(
                    r#"
[badge.git]
type = "builtin"
name = "{builtin}"
{field}{scope_config}
"#
                );
                let path = write_temp(&toml);
                let result = Config::load(&path);

                if accepted {
                    let cfg = result.unwrap_or_else(|err| {
                        panic!("{builtin} with {scope_case} scope was rejected: {err}")
                    });
                    assert_eq!(
                        cfg.badge["git"].scope,
                        Scope::GitRoot,
                        "{builtin} with {scope_case} scope"
                    );
                } else {
                    let err = result.expect_err(&format!(
                        "{builtin} with {scope_case} scope should be rejected"
                    ));
                    assert!(
                        err.contains("git builtins require `scope = \"git_root\"`"),
                        "{builtin} error was not actionable: {err}"
                    );
                }
            }
        }
    }

    #[test]
    fn scope_can_be_overridden_for_non_git_builtins_commands_and_http() {
        let toml = r#"
[badge.hostname]
type = "builtin"
name = "hostname"
scope = "git_root"

[badge.battery]
type = "builtin"
name = "battery"
scope = "global"

[badge.repo_ls]
type = "command"
command = "ls"
scope = "git_root"

[badge.public_ip]
type = "http"
url = "https://example.com/ip"
scope = "git_root"
"#;
        let path = write_temp(toml);
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.badge["hostname"].scope, Scope::GitRoot);
        assert_eq!(cfg.badge["battery"].scope, Scope::Global);
        assert_eq!(cfg.badge["repo_ls"].scope, Scope::GitRoot);
        assert_eq!(cfg.badge["public_ip"].scope, Scope::GitRoot);
    }

    #[test]
    fn scope_rejects_unknown_value() {
        let toml = r#"
[badge.a]
type = "command"
command = "echo"
scope = "repo"
"#;
        let path = write_temp(toml);
        assert!(Config::load(&path).is_err());
    }
}
