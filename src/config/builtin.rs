//! Builtin provider names, git fields and per-tool `tool_version` defaults (pure types; the
//! `tool_version` expansion lives in `raw.rs`).

use serde::Deserialize;

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

/// `name = ...` of a `type = "builtin"` badge: a runtime `Builtin`, or the `tool_version`
/// sugar that expands to a `project_root` command badge at load (see `Tool`) and so never
/// reaches `Source::Builtin`.
#[derive(Debug, Clone, Copy)]
pub(super) enum BuiltinName {
    Runtime(Builtin),
    ToolVersion,
}

impl<'de> Deserialize<'de> for BuiltinName {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::IntoDeserializer;
        let name = String::deserialize(d)?;
        if name == "tool_version" {
            return Ok(Self::ToolVersion);
        }
        Builtin::deserialize(name.as_str().into_deserializer()).map(Self::Runtime)
    }
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

    /// Backed by one shared `git status --porcelain=v2 --branch` subprocess per repo.
    pub const fn uses_porcelain(self) -> bool {
        matches!(self, Self::GitStatus | Self::GitCounts)
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

/// `tool = ...` of `name = "tool_version"`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Tool {
    Node,
    Python,
    Rust,
    Go,
    Ruby,
}

/// Watched by every `tool_version` badge, in addition to the tool's markers.
pub(super) const COMMON_WATCH: &[&str] = &[".tool-versions", "mise.toml"];
/// Forwarded for every `tool_version` badge, in addition to the tool's own variables.
pub(super) const COMMON_ENV: &[&str] = &["PATH", "MISE_*", "ASDF_*"];

/// Per-tool `tool_version` defaults.
pub(super) struct ToolDefaults {
    pub(super) program: &'static str,
    pub(super) version_arg: &'static str,
    /// Also the `when_file` and `watch` defaults.
    pub(super) markers: &'static [&'static str],
    /// Version-manager env vars.
    pub(super) env: &'static [&'static str],
}

impl Tool {
    /// Limitation: version files aren't parsed (the shims resolve them and `watch` handles
    /// invalidation); we always run the tool's version command.
    pub(super) const fn defaults(self) -> ToolDefaults {
        let (program, version_arg, markers, env): (_, _, &[_], &[_]) = match self {
            Self::Node => (
                "node",
                "--version",
                &[".nvmrc", ".node-version", "package.json"],
                &["NVM_BIN", "VOLTA_HOME"],
            ),
            Self::Python => (
                "python3",
                "--version",
                &[".python-version", "pyproject.toml"],
                &["PYENV_VERSION", "VIRTUAL_ENV"],
            ),
            Self::Rust => (
                "rustc",
                "--version",
                &["rust-toolchain.toml", "rust-toolchain", "Cargo.toml"],
                &["RUSTUP_TOOLCHAIN"],
            ),
            // Go has no `--version`.
            Self::Go => ("go", "version", &["go.mod"], &["GOROOT"]),
            Self::Ruby => (
                "ruby",
                "--version",
                &[".ruby-version", "Gemfile"],
                &["RBENV_VERSION"],
            ),
        };
        ToolDefaults {
            program,
            version_arg,
            markers,
            env,
        }
    }
}
