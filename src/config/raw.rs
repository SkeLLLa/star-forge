//! Serde-facing badge config, converted to `Source` + common fields at load.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::builtin::{BuiltinName, COMMON_ENV, COMMON_WATCH, GitField, Tool};
use super::{Builtin, PathOpts, Scope, Source};
use crate::extract::RawExtract;

fn default_format() -> String {
    "{value}".to_string()
}

/// One `[badge.<name>]` entry, deserialized directly (no `#[serde(flatten)]`): each variant
/// duplicates the handful of common fields so `deny_unknown_fields` actually catches config
/// typos (flatten + `deny_unknown_fields` don't combine in serde — see design.md §4).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum RawBadgeConfig {
    Builtin {
        name: BuiltinName,
        /// `battery`-only option; rejected on every other builtin (`into_parts`).
        device: Option<String>,
        /// `git_counts`-only option; required for it, rejected on every other builtin
        /// (`into_parts`).
        field: Option<GitField>,
        #[serde(default = "default_format")]
        format: String,
        interval: Option<String>,
        battery_interval: Option<String>,
        active_window: Option<String>,
        timeout: Option<String>,
        max_output: Option<usize>,
        extract: Option<RawExtract>,
        scope: Option<Scope>,
        /// `tool_version`-only options (`into_parts`); each overrides the tool's default.
        tool: Option<Tool>,
        command: Option<String>,
        args: Option<Vec<String>>,
        markers: Option<Vec<String>>,
        watch: Option<Vec<String>>,
        when_file: Option<Vec<String>>,
        env: Option<Vec<String>>,
    },
    Command {
        command: String,
        args: Option<Vec<String>>,
        #[serde(default = "default_format")]
        format: String,
        interval: Option<String>,
        battery_interval: Option<String>,
        active_window: Option<String>,
        timeout: Option<String>,
        max_output: Option<usize>,
        extract: Option<RawExtract>,
        scope: Option<Scope>,
        #[serde(default)]
        markers: Vec<String>,
        #[serde(default)]
        watch: Vec<String>,
        #[serde(default)]
        when_file: Vec<String>,
        #[serde(default)]
        env: Vec<String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default = "default_format")]
        format: String,
        interval: Option<String>,
        battery_interval: Option<String>,
        active_window: Option<String>,
        timeout: Option<String>,
        max_output: Option<usize>,
        extract: Option<RawExtract>,
        scope: Option<Scope>,
    },
}

/// Fields shared by every badge type, pulled out of whichever `RawBadgeConfig` variant
/// matched (see `RawBadgeConfig::into_parts`).
pub(super) struct RawCommon {
    pub(super) format: String,
    pub(super) interval: Option<String>,
    pub(super) battery_interval: Option<String>,
    pub(super) active_window: Option<String>,
    pub(super) timeout: Option<String>,
    pub(super) max_output: Option<usize>,
    pub(super) extract: Option<RawExtract>,
    pub(super) scope: Option<Scope>,
    pub(super) path: PathOpts,
}

impl RawBadgeConfig {
    /// Splits into the provider-specific `Source` and the shared timing/format fields.
    /// Validates builtin-specific options against the wrong builtin (e.g. `device` on
    /// `git_branch`), since serde can't express that conditionally.
    pub(super) fn into_parts(self, badge_name: &str) -> Result<(Source, RawCommon), String> {
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
                tool,
                command,
                args,
                markers,
                watch,
                when_file,
                env,
            } => {
                let tool_opts = ToolOpts {
                    tool,
                    command,
                    args,
                    markers,
                    watch,
                    when_file,
                    env,
                };
                let common = RawCommon {
                    format,
                    interval,
                    battery_interval,
                    active_window,
                    timeout,
                    max_output,
                    extract,
                    scope,
                    path: PathOpts::default(),
                };
                let name = match name {
                    BuiltinName::ToolVersion => {
                        if device.is_some() || field.is_some() {
                            return Err(format!(
                                "badge {badge_name}: `device`/`field` are not valid for \
                                 `tool_version`"
                            ));
                        }
                        return expand_tool_version(badge_name, tool_opts, common);
                    }
                    BuiltinName::Runtime(name) => name,
                };
                validate_builtin_opts(
                    badge_name,
                    name,
                    device.is_some(),
                    field.is_some(),
                    &tool_opts,
                )?;
                Ok((
                    Source::Builtin {
                        name,
                        device,
                        field,
                    },
                    common,
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
                markers,
                watch,
                when_file,
                env,
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
                    path: PathOpts {
                        env,
                        markers,
                        watch,
                        when_file,
                    },
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
                    path: PathOpts::default(),
                },
            )),
        }
    }
}

/// Rejects options that don't belong to the runtime builtin `name` (`device` only on
/// `battery`, `field` only, and required, on `git_counts`, tool options only on `tool_version`).
fn validate_builtin_opts(
    badge_name: &str,
    name: Builtin,
    has_device: bool,
    has_field: bool,
    tool_opts: &ToolOpts,
) -> Result<(), String> {
    if tool_opts.any() {
        return Err(format!(
            "badge {badge_name}: `tool`, `command`, `args`, `markers`, `watch`, \
             `when_file` and `env` are only valid for `name = \"tool_version\"`"
        ));
    }
    if has_device && name != Builtin::Battery {
        return Err(format!(
            "badge {badge_name}: `device` is only valid for `type = \"builtin\"` \
             `name = \"battery\"`"
        ));
    }
    if has_field && name != Builtin::GitCounts {
        return Err(format!(
            "badge {badge_name}: `field` is only valid for `type = \"builtin\"` \
             `name = \"git_counts\"`"
        ));
    }
    if !has_field && name == Builtin::GitCounts {
        return Err(format!(
            "badge {badge_name}: `type = \"builtin\"` `name = \"git_counts\"` \
             requires `field`"
        ));
    }
    Ok(())
}

/// `tool_version`-only options of a builtin badge; each overrides the tool's default.
pub(super) struct ToolOpts {
    pub(super) tool: Option<Tool>,
    pub(super) command: Option<String>,
    pub(super) args: Option<Vec<String>>,
    pub(super) markers: Option<Vec<String>>,
    pub(super) watch: Option<Vec<String>>,
    pub(super) when_file: Option<Vec<String>>,
    pub(super) env: Option<Vec<String>>,
}

impl ToolOpts {
    pub(super) const fn any(&self) -> bool {
        self.tool.is_some()
            || self.command.is_some()
            || self.args.is_some()
            || self.markers.is_some()
            || self.watch.is_some()
            || self.when_file.is_some()
            || self.env.is_some()
    }
}

/// Expands `name = "tool_version"` into a `project_root` command badge running the tool's
/// version command, filling every unset option from `Tool::defaults`.
pub(super) fn expand_tool_version(
    badge_name: &str,
    opts: ToolOpts,
    common: RawCommon,
) -> Result<(Source, RawCommon), String> {
    let ToolOpts {
        tool,
        command,
        args,
        markers,
        watch,
        when_file,
        env,
    } = opts;
    let tool = tool
        .ok_or_else(|| format!("badge {badge_name}: `name = \"tool_version\"` requires `tool`"))?;
    if common.scope.is_some_and(|s| s != Scope::ProjectRoot) {
        return Err(format!(
            "badge {badge_name}: `name = \"tool_version\"` requires `scope = \"project_root\"`"
        ));
    }
    let d = tool.defaults();
    let own = |l: &[&str]| l.iter().map(ToString::to_string).collect::<Vec<_>>();
    let markers = markers.unwrap_or_else(|| own(d.markers));
    Ok((
        // A custom `command` without `args` runs via `sh -c`, as on command badges.
        Source::Command {
            args: if command.is_some() {
                args
            } else {
                args.or_else(|| Some(vec![d.version_arg.to_string()]))
            },
            command: command.unwrap_or_else(|| d.program.to_string()),
        },
        RawCommon {
            extract: common.extract.or_else(|| {
                Some(RawExtract::Regex {
                    pattern: r"\d+\.\d+(\.\d+)?".to_string(),
                    group: 0,
                })
            }),
            scope: common.scope.or(Some(Scope::ProjectRoot)),
            path: PathOpts {
                when_file: when_file.unwrap_or_else(|| markers.clone()),
                watch: watch.unwrap_or_else(|| {
                    let mut w = own(d.markers);
                    w.extend(own(COMMON_WATCH));
                    w
                }),
                env: env.unwrap_or_else(|| {
                    let mut e = own(COMMON_ENV);
                    e.extend(own(d.env));
                    e
                }),
                markers,
            },
            ..common
        },
    ))
}
