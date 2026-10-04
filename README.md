# star-forge

[![CI](https://github.com/SkeLLLa/star-forge/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/SkeLLLa/star-forge/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SkeLLLa/star-forge)](https://github.com/SkeLLLa/star-forge/releases/latest)
[![crates.io](https://img.shields.io/crates/v/star-forge)](https://crates.io/crates/star-forge)
[![MSRV](https://img.shields.io/crates/msrv/star-forge)](Cargo.toml)
[![License: GPL-3.0-or-later](https://img.shields.io/crates/l/star-forge)](COPYING)

Fast, cached badges for your shell prompt and tmux status line.

[Starship](https://starship.rs) is great at drawing a prompt, but it starts from scratch every
time it draws one. Its built-in modules are quick. A `[custom.*]` module is a different story:
your public IP, the weather, CI status, or the output of some slow CLI all run again every time
you press Enter. A 300 ms `curl` means a 300 ms pause before every prompt. If a command takes
longer than starship's `command_timeout` (500 ms by default), starship kills it, so the module
doesn't show and a warning goes to the log.

star-forge takes the slow work out of the prompt. A small background daemon runs your
commands on their own schedule and keeps the latest results. Your prompt reads the cached value
with `stfg`, which usually answers in under a millisecond and never takes longer than 40 ms.
Anything that can run a command can use it: starship, tmux, or your own statusline script.

## What you get

- **A prompt that never waits.** Slow commands and HTTP requests refresh in the background,
  and the prompt shows the last known value right away.
- **No error noise.** If a command fails or times out, or the daemon isn't running yet, the
  badge is simply empty. Nothing is ever printed to your terminal.
- **Each command runs once, not once per shell.** Ten terminals and a tmux status bar all read
  the same cache, so a weather API gets one request every 30 minutes instead of one per prompt
  in every pane.
- **Up-to-date git info at almost no cost.** Branch, commit, rebase/merge state and stash count
  are read straight from `.git`, without starting a subprocess. Counts that do need
  `git status` are cached per repository and refreshed as soon as you stage, commit or switch
  branches.
- **Fewer processes per prompt.** A group renders several badges from one `stfg` call. It can
  even style them with starship's own format syntax and your starship palette. In the
  [example below](#batching-calls), nine custom modules become two.
- **Easy on your battery.** Badges refresh only while something is using them, refreshes are
  batched onto a single timer, and you can set slower intervals for when you're on battery. The
  daemon quits after 30 idle minutes and starts again the next time a badge is requested.
- **Works with tmux too.** The same badges can be rendered in tmux's own `#[fg=...]` style
  syntax.

## Is it for me?

It probably is if your prompt has custom modules that make network calls or run slow tools, or
if you notice a lag after pressing Enter in big repositories. If you only use starship's
built-in modules and your prompt already feels instant, you don't need star-forge.

## Quick start

1. Install it (see [Install](#install)).
2. Describe a badge in `~/.config/star-forge/config.toml`:

   ```toml
   [badge.public_ip]
   type = "http"
   url = "https://api.ipify.org?format=json"
   extract = { kind = "json", pointer = "/ip" }
   interval = "10m"
   ```

3. Show it in `~/.config/starship.toml`:

   ```toml
   [custom.sf_public_ip]
   command = "public_ip"
   shell = ["stfg"]
   use_stdin = false
   when = true
   format = "([$output ]($style))"
   ```

4. Open a new prompt. The very first one may show nothing while the value is fetched. After
   that the value is always there, and `stfgd status` shows what's cached.

You don't need to start anything yourself: the daemon is launched automatically the first time
a badge is requested. If you're curious how it works inside, see
[`docs/design.md`](docs/design.md).

## Install

```sh
cargo install star-forge        # or: cargo install --path .
```

Each GitHub release also ships Linux (`x86_64` glibc, plus a fully static `x86_64` musl build)
and macOS (`aarch64`, `x86_64`) tarballs plus `.deb`/`.rpm` packages. The packages are also
published as APT/DNF repositories on GitHub Pages (<https://skellla.github.io/star-forge>).

For Fedora, openSUSE, and other RPM-based systems:

```sh
sudo tee /etc/yum.repos.d/star-forge.repo >/dev/null <<'EOF'
[star-forge]
name=star-forge
baseurl=https://skellla.github.io/star-forge/rpm/x86_64
enabled=1
gpgcheck=0
repo_gpgcheck=0
EOF
sudo dnf install star-forge
```

For Debian, Ubuntu, and other APT-based systems:

```sh
echo 'deb [trusted=yes] https://skellla.github.io/star-forge/deb stable main' | sudo tee /etc/apt/sources.list.d/star-forge.list
sudo apt update
sudo apt install star-forge
```

For Nix (flakes), on Linux and macOS:

```sh
nix profile install github:SkeLLLa/star-forge
nix run github:SkeLLLa/star-forge -- --help
```

On Linux the package also ships the systemd user unit under `lib/systemd/user/`.

For [`mise`](https://mise.jdx.dev), on Linux and macOS:

```sh
mise use -g github:SkeLLLa/star-forge
```

To pin a specific release: `mise use -g github:SkeLLLa/star-forge@1.0.0`. Releases from 1.1.0
on also publish a signed `packslip.sigstore.json` manifest. This installs both binaries but no
systemd unit; see the first-run notes below.

The repositories are unsigned. See [`docs/distribution.md`](docs/distribution.md) for artifact
contents, the release pipeline, and first-run steps per install method.

Installs two binaries: `stfgd` (CLI + daemon) and `stfg` — a tiny std+libc-only client for
just the `get` hot path (no tokio/serde/reqwest, smaller and slightly faster to start than
`stfgd get`; see `docs/design.md` §3). No setup is required to run either — a missing config
file is treated as empty (no badges configured), and the daemon is spawned automatically on
first use.

The `.deb`/`.rpm` packages also install a systemd user unit,
`/usr/lib/systemd/user/star-forge.service`, to start the daemon with your session instead of on
the first `get`:

```sh
systemctl --user enable --now star-forge.service
```

The daemon still honours `daemon.idle_exit`: after that long without requests it exits cleanly,
the unit goes inactive, and the next `get` spawns a daemon outside systemd as usual. Set
`idle_exit` to a long duration (e.g. `"8760h"`) to keep the unit running for the whole session.
If a daemon is already running when the unit starts, the unit exits immediately (the daemon is
a per-user singleton); run `stfgd stop` first. For a `cargo install`, see
[`docs/distribution.md`](docs/distribution.md) for a unit pointing at `~/.cargo/bin/stfgd`.

## Usage

```sh
stfgd get <badge>... [--cwd <path>]   # hot path: never fails, no stderr
stfgd daemon                          # foreground (used internally; run instead by `get`)
stfgd status                          # cached badges, scope, age, errors; last config error
stfgd stop
stfgd reload
```

If the daemon isn't running, exit codes follow LSB init scripts (as `systemctl` does): `status`
exits 3, `reload` exits 7, and `stop` succeeds (0). An unresponsive daemon exits 1.

`stfg <badge>... [--cwd <path>]` is a separate, tiny binary that is an exact equivalent to
`stfgd get` — same implementation, same flags, same behavior, just without the full binary's
dependency weight. Prefer it for the hot path (starship/tmux); use `stfgd get` only where
installing a second binary isn't convenient.

## Config

Path: `$STAR_FORGE_CONFIG`, else `$XDG_CONFIG_HOME/star-forge/config.toml`, else
`~/.config/star-forge/config.toml`.

A config that fails to parse or fails validation is rejected wholesale and logged to
stderr only (never surfaced to a client): on daemon startup this means running with zero
badges until it's fixed; a bad `stfgd reload` instead keeps whatever config was already
running.

Every timing value is a string `<integer><unit>` with unit `ms|s|m|h` (e.g. `"500ms"`,
`"60s"`, `"5m"`, `"1h"`).

```toml
[daemon]
idle_exit = "30m"          # default 30m: exit after this long with no client requests
interval = "60s"           # default 60s: fallback refresh period for badges that don't set their own
battery_interval = "60s"   # default: same as `interval`; fallback refresh period while on battery
active_window = "5m"       # default 5m: a badge is kept refreshed for this long after its last use
coalesce = "1s"            # default 1s: batch refreshes due within this long of each other; must be < every resolved interval
retry_min = "2s"           # default 2s: backoff after a failed fetch starts here, doubling on each failure; must be > 0
retry_max = "5m"           # default 5m: backoff cap
timeout = "2s"             # default 2s: fallback hard kill for the whole fetch + extract; must be > 0
power_check = "60s"        # default 60s: how often AC/battery state is re-checked
config_check = "2s"        # default 2s: how often the config and palette_from files are stat'ed for an implicit reload
cold_wait = "20ms"         # default 20ms: how long a cold (no value yet) `get` waits for its own fetch; must be < 40ms
path_evict = "30m"         # default 30m: idle git-scoped badges are dropped after this long
max_paths = 256            # default 256: LRU cap on git-scoped (per-repo) cache entries
max_output = 65536         # default 64 KiB: fallback cap on command stdout / HTTP body
palette_from = "~/.config/starship.toml"   # optional: import the active starship palette (see Styled groups)

[palette]                  # optional: inline color names for group styles; overrides palette_from
# color_bg_l1 = "#3b4252"

[badge.<name>]
type = "builtin" | "command" | "http"
format = "{value}"             # default "{value}"; the one placeholder is replaced with the raw value
interval = "5s"                 # optional; defaults to [daemon] interval. Refresh period (global) / freshness TTL (git-scoped)
battery_interval = "5s"         # optional; resolution order below
active_window = "5m"            # optional; defaults to [daemon] active_window
timeout = "2s"                  # optional; defaults to [daemon] timeout
max_output = 65536              # optional; defaults to [daemon] max_output
scope = "git_root" | "project_root" | "global"   # optional; defaults to git_root for git builtins, global otherwise
markers = [".nvmrc"]            # command/tool_version badges; required with (only valid for) scope = "project_root"
watch = [".nvmrc"]              # command/tool_version badges, path scope only; paths relative to the scope root
when_file = ["package.json"]    # command/tool_version badges, path scope only; empty unless one exists in the scope root
env = ["PATH", "NVM_BIN", "MISE_*"]   # command/tool_version badges; variables taken from the requesting client (trailing * = prefix)
extract = { kind = "trim" | "regex" | "json", ... }   # default: trim
```

Every badge-level knob above falls back to its `[daemon]` counterpart when unset, except
`battery_interval`, which has its own four-level chain: badge `battery_interval` → badge
`interval` (if set) → daemon `battery_interval` → daemon `interval`. Unknown fields in
`[daemon]` or any `[badge.*]` are rejected at load.

`type = "builtin"` badges (`name = "git_branch" | "git_status" | "git_counts" | "git_commit" |
"git_state" | "git_stash" | "battery" | "hostname" | "load_avg" | "mem_used_percent" |
"uptime" | "tool_version"`):

- `git_branch`, `git_commit`, `git_state`, `git_stash` are computed in-process from files
  under `.git` on every request — no subprocess, no cache, so they're always exactly as
  fresh as `HEAD`/the ref files on disk (no stale-while-revalidate window at all).
- `git_status` and `git_counts` still shell out to `git status`, but their cached value is
  invalidated the instant `.git/HEAD` or `.git/index` changes (e.g. `git add`, `git commit`,
  `git checkout -b`), not just on `interval`. Unstaged worktree edits touch neither file, so
  those are still only picked up on `interval`. `git_counts` requires
  `field = "ahead" | "behind" | "staged" | "modified" | "untracked" | "conflicted"`
  (rejected on every other builtin); concurrent `git_counts` badges on the same repo share
  one `git status` subprocess instead of one each.
- `battery`/`hostname`/`load_avg`/`mem_used_percent`/`uptime` are global (host-wide, not
  per repo).
- `tool_version` runs a tool's version command per project root; see
  [`tool_version`](#tool_version).

Overriding builtins: every builtin takes the common knobs (`format`, `interval`, `timeout`,
`extract`, `scope` where allowed, …), so its output, refresh rate and parsing are yours to
change. `tool_version` is a preset command badge, so all of its defaults (the version command
via `command`/`args`, plus `markers`/`watch`/`when_file`/`env`/`extract`/`scope`) can be
replaced. The other builtins are computed in-process (no command to swap); to replace one
entirely, define a `command` badge under the same name instead.

All six git builtins require `scope = "git_root"` (also their default): they use the repo
containing `--cwd`, and render empty outside a repo without running a subprocess.
`scope = "global"` on a git builtin is rejected at config load rather than silently ignored.
Everything else defaults to global and can override this with
`scope = "git_root" | "global"` (`project_root` for command and `tool_version` badges).
A `command` badge scoped to `git_root` is run with
cwd = the repo root and cached per repo, same as a git builtin.

`command` badges can also use `scope = "project_root"` with a required `markers` list: the
root is the nearest ancestor of `--cwd` (itself included) containing any marker, searched up
to and including `$HOME` (or `/` when `--cwd` is outside `$HOME`). The command runs there and
is cached per root (same `max_paths`/`path_evict` handling as `git_root`); with no match the
badge renders empty and nothing is spawned. This fixes monorepos (`sub/.nvmrc`) and non-git
directories.

On `command` badges with a path scope (`git_root` or `project_root`):

- `watch = ["rel/path", ...]` refreshes the cached value on the next request after any listed
  file changes (inode, size, ctime; a missing file counts, so creating or deleting one also
  triggers it), like the git index trigger: a cold-value-style refresh that waits up to
  `cold_wait`, rather than waiting out `interval`.
- `when_file = ["rel/path", ...]` renders empty, without spawning, unless at least one of the
  files exists in the scope root. This lets one group hold every language badge.

`watch`/`when_file` are rejected with `scope = "global"`, and `markers` with any scope but
`project_root`.

```toml
[badge.node]
type = "command"
command = "node --version"
scope = "project_root"
markers = [".nvmrc", "package.json", ".tool-versions"]
watch = [".nvmrc", ".tool-versions", "mise.toml"]   # edit one -> refreshed on the next prompt
when_file = ["package.json", ".nvmrc"]              # not a node project -> empty, no spawn
interval = "10m"
```

`env = ["PATH", "NVM_BIN", "MISE_*"]` on a `command` badge runs it with those variables taken
from the client that asked (a trailing `*` is a prefix glob, e.g. `MISE_*`); every other
variable comes from the daemon's environment. The selected `(name, value)` pairs are part of
the cache key (next to the scope root), so after `nvm use` / `mise use` the next prompt gets
its own entry and shows the new version immediately. Such entries are evicted like path-scoped
ones (`max_paths`/`path_evict`) and are refreshed on request only, never by the timer. The
client always sends its whole environment (a few KB over the socket) and the daemon filters
per badge, so `stfg` never reads the config. A client that sends none (an older `stfg`) gets
the daemon's environment. Variables the client lacks do not fall back to the daemon's: for a
request that carried an environment, matching variables missing from it are unset for the
command (so `VIRTUAL_ENV` after `deactivate` is really gone). `stfg` skips single variables
over 4 KiB (exported shell functions, `LS_COLORS`), and drops the whole block only if the
request would still exceed ~60 KiB. Values are never logged.

### `tool_version`

`type = "builtin"`, `name = "tool_version"` and `tool = "node" | "python" | "rust" | "go" |
"ruby"` is sugar for the `project_root` command badge above: it runs the tool's version
command (`node --version`, `python3 --version`, `rustc --version`, `go version`,
`ruby --version`) in the project root and extracts the first `\d+\.\d+(\.\d+)?`.
Every default is overridable: `command`/`args` replace the version command (a `command`
without `args` runs via `sh -c`, as on command badges), `extract`/`scope` work as usual, and
`markers`/`watch`/`when_file`/`env` replace the lists below:

| tool | `markers` (= `when_file`) | extra `env` |
| --- | --- | --- |
| node | `.nvmrc`, `.node-version`, `package.json` | `NVM_BIN`, `VOLTA_HOME` |
| python | `.python-version`, `pyproject.toml` | `PYENV_VERSION`, `VIRTUAL_ENV` |
| rust | `rust-toolchain.toml`, `rust-toolchain`, `Cargo.toml` | `RUSTUP_TOOLCHAIN` |
| go | `go.mod` | `GOROOT` |
| ruby | `.ruby-version`, `Gemfile` | `RBENV_VERSION` |

`watch` is the markers plus `.tool-versions` and `mise.toml` for every tool, and `env` is
`PATH`, `MISE_*`, `ASDF_*` plus the extra variables. Version files are not parsed: the shims
resolve them, and `watch` refreshes the value when one changes.

```toml
[badge.node]
type = "builtin"
name = "tool_version"
tool = "node"
format = " {value}"
interval = "10m"

[badge.python]
type = "builtin"
name = "tool_version"
tool = "python"
format = " {value}"
interval = "10m"

[badge.rust]
type = "builtin"
name = "tool_version"
tool = "rust"
format = " {value}"
interval = "10m"

# Overriding the version command, e.g. a uv-managed interpreter.
[badge.uv_python]
type = "builtin"
name = "tool_version"
tool = "python"
command = "uv"
args = ["run", "python", "--version"]

# One stfg call (and one starship module) for every language badge; the ones whose
# `when_file` isn't met render empty and are skipped.
[groups.langs]
badges = ["node", "python", "rust"]
separator = " "
```

```toml
# starship.toml
[custom.sf_langs]
command = "langs"
shell = ["stfg"]
use_stdin = false
when = true
format = '$output'
```

`battery` is the charge percent (no `%` sign). On Linux it reads
`/sys/class/power_supply/*/capacity`; the optional `device = "BAT0"` picks the entry
(rejected on every other builtin), otherwise the first entry whose `type` is `Battery` is
used. On macOS it runs `/usr/bin/pmset -g batt` (bounded by the badge `timeout`, killed with
its process group like any command); `device` is a power-source name exactly as `pmset`
prints it (e.g. `InternalBattery-0` or a UPS name), otherwise the first `InternalBattery*`
source is used. No battery (desktop) or no matching `device` renders empty. `git_status`'s
raw value is the number of changed entries (staged, modified, untracked, conflicted; each
counted once), from the same `git status --porcelain=v2` run `git_counts` uses, rendered empty
(so `format` also renders empty) when the tree is clean.
`git_commit` is the first 7 characters of `HEAD`'s commit sha (empty on an unborn branch).
`git_state` is one of `REBASING`/`MERGING`/`CHERRY-PICKING`/`BISECTING`, empty otherwise.
`git_stash` is the stash entry count, empty when there are none. `load_avg` is
the 1/5/15-minute load averages (`getloadavg(3)`, two decimals each), space-separated.
`mem_used_percent` is used memory as a whole-number percent of total, with file cache and
purgeable memory counted as free: `(MemTotal - MemAvailable) / MemTotal` from `/proc/meminfo`
on Linux; on macOS `host_statistics64` pages (internal − purgeable + wired + compressed) over
`hw.memsize`, matching Activity Monitor's "Memory Used". `uptime` is system uptime in whole
seconds, including time asleep (`CLOCK_BOOTTIME` on Linux, `kern.boottime` on macOS).
`hostname` is `gethostname(3)` verbatim, i.e. what `hostname` prints — often `<name>.local` on
macOS; for the short form use `extract = { kind = "regex", pattern = "^[^.]+", group = 0 }`.

Linux and macOS are supported, with every builtin available on both. `git_status` and
`git_counts` need a working `git` on `PATH` (on macOS, `/usr/bin/git` without the Xcode
Command Line Tools fails, so they render empty). Power detection for `battery_interval` is
re-checked every `power_check`: on Linux you're on AC if any non-`Battery` supply reports
`online == 1` (or there are no supplies at all); on macOS it follows the `Now drawing from`
header of `pmset -g batt`, treating a failed or slow (>400 ms) probe as AC.

`type = "command"` runs `command`. If `args` is given, it's exec'd directly with those args
(no shell). If `args` is absent, `command` is run as `sh -c <command>`, so pipelines and
`&&` work (the whole pipeline is one process group, killed together on timeout).
`type = "http"` fetches `url` with optional `headers`.

`extract`:

- `{ kind = "trim" }` (default) — trim whitespace.
- `{ kind = "regex", pattern = "...", group = N }` — first match's group `N`.
- `{ kind = "json", pointer = "/a/b" }` — `serde_json` pointer into the parsed body.

### Example

```toml
[daemon]
idle_exit = "30m"

[badge.git_branch]
type = "builtin"
name = "git_branch"
format = " {value}"
interval = "3s"

[badge.git_status]
type = "builtin"
name = "git_status"
interval = "3s"

[badge.git_ahead]
type = "builtin"
name = "git_counts"
field = "ahead"
interval = "3s"

[badge.git_stash]
type = "builtin"
name = "git_stash"

[badge.git_state]
type = "builtin"
name = "git_state"

[badge.uptime]
type = "builtin"
name = "uptime"
interval = "60s"

[badge.battery]
type = "builtin"
name = "battery"
format = "󰚥 {value}%"
interval = "60s"

[badge.public_ip]
type = "http"
url = "https://api.ipify.org?format=json"
interval = "10m"
timeout = "3s"
format = " {value}"
extract = { kind = "json", pointer = "/ip" }

[badge.weather]
type = "command"
command = "curl -sf https://wttr.in/?format=j1 | jq -r '.current_condition[0].temp_C'"
interval = "30m"
timeout = "5s"
format = " {value}°C"

[badge.kernel]
type = "command"
command = "uname"
args = ["-r"]
interval = "24h"
format = " {value}"
extract = { kind = "regex", pattern = '^(\d+\.\d+)', group = 1 }
```

### Groups

A group renders several badges on one line, from a single `stfg` call (tmux `#()` shows only
the first output line):

```toml
[groups.right]
badges = ["battery", "uptime", "git_branch"]
separator = " "        # optional, default " "
```

`stfg right` resolves each member as if requested individually (same cwd/scope handling,
staleness, `cold_wait`), then joins the non-empty values with `separator`; all empty ⇒ empty
line. A group name must not collide with a badge name, members must be defined badges (no
nesting), the list must be non-empty, and `separator` must not contain newline, CR, or 0x1F.
Group changes are picked up by config reload like any other.

#### Styled groups

Instead of `badges`/`separator`, a group may set `format` (mutually exclusive with both),
using starship's format syntax:

- `{badge}` — the badge's value (empty if it has none).
- `[text](style)` — a styled span; spans nest.
- `( ... )` — a conditional section, dropped (padding and all) when every badge inside is empty.
- Escapes: `\[ \] \( \) \{ \} \\`.

Styles are starship style strings: `fg:`/`bg:`/bare color, `#rrggbb`, named colors (`black red
green yellow blue purple cyan white`, `bright-*`), 0-255, or palette names, plus modifiers
`bold italic underline dimmed inverted blink hidden strikethrough none`. Output is raw ANSI
unless the group sets `output = "tmux"` (see [tmux](#tmux)). A `( ... )` section containing no
`{badge}` at all is never rendered (mirrors starship).

Palette names come from `[daemon] palette_from`, which imports the active palette of a
starship config (`palette = "<name>"` plus `[palettes.<name>]`); an inline `[palette]` table in
the star-forge config overrides it. The palette file is watched along with the
config: editing it is picked up automatically within `config_check` (or run `stfgd reload`).
Changes are detected by inode, size and ctime rather than mtime alone, so `rsync -a`/`cp -p`
copies and Nix/home-manager symlink switches are noticed too. On a filesystem that doesn't
report this metadata (some FUSE mounts), run `stfgd reload` yourself, e.g. from a
home-manager activation hook or a chezmoi `run_after_` script.

The template is parsed once at config load; per request only badge substitution happens.

Worked example: replace several per-badge starship modules (each with powerline separators)
by one module per group. Copy the chunk of your starship `format`, replacing `${custom.sf_x}`
with `{x}`:

```toml
[daemon]
palette_from = "~/.config/starship.toml"

[groups.git]
format = """
([  {git_branch} ](bg:color_bg_l2 fg:color_fg_l2))\
([{git_conflicted} ](bg:color_bg_l2 fg:color_fg_l2))\
([{git_staged} ](bg:color_bg_l2 fg:color_fg_l2))\
([{git_modified} ](bg:color_bg_l2 fg:color_fg_l2))\
"""

[groups.right]
format = """
([󰩠 {public_ip} ](bg:color_bg_l2 fg:color_fg_l2))\
[](bg:color_bg_l2 fg:color_bg_l1)\
([󰚥 {battery}% ](fg:color_fg_l1 bg:color_bg_l1))\
"""
```

and on the starship side:

```toml
[custom.sf_right]
command = "right"
shell = ["stfg"]
use_stdin = false
when = true
format = '$output'
```

Use `format = '$output'` and no module `style`: starship applies `style` once as a prefix, and
any reset in the output cancels it, so the group carries all of its own styling. Starship wraps
the ANSI for bash/zsh itself and measures width correctly (verified on starship 1.26); it does
not interpret `[..](..)` markup in custom output, which is why star-forge renders it.

This also cuts process spawns: one `stfg` per module, not per badge (e.g. 9 → 2).

TOML note: in basic strings (`"..."`, `"""..."""`) a trailing `\` at line end trims the newline
and following whitespace (wanted above), but `\[` is an invalid TOML escape. To get a literal
`\[` in the template write `\\[`, or use literal strings (`'''...'''`).

For tmux, set `output = "tmux"` on the group instead; tmux does not render ANSI (see below).

## starship

```toml
[custom.sf_public_ip]
command = "public_ip"                 # badge name(s), appended as argv
shell = ["stfg"]                       # tiny std+libc client, no `sh -c` layer
use_stdin = false
when = true                           # custom modules are hidden by default without when/detect_*
format = "([$output ]($style))"      # `( )` drops the group, padding included, on empty output
style = "bg:color_bg_l1 fg:color_fg_l1"
```

Running `stfg` as the "shell" skips both the per-module `sh -c` fork and the full
binary's larger startup cost. Starship runs custom commands in the prompt's cwd, so git badges
resolve correctly.

Empty output ⇒ `$output` is empty, and the conditional `( … )` group drops the module's text,
including padding/icons; a plain `[$output ]` group would still render its literal space.
One `[custom.*]` block per badge; verified against starship 1.26. To show several badges in
one module, define a group and use its name: `command = "right"`.

## tmux

tmux does **not** render ANSI escapes in `#()` output (the ESC byte is dropped and `[31m` shows
up literally). It does re-expand the output as a format string: `#[fg=...]` styles work, and
every literal `#` must be written `##`. Set `output = "tmux"` on a group (plain or `format`) to
get that dialect:

```toml
[groups.right_tmux]
output = "tmux"
format = """
[ {git_branch} ](fg:#5a5a5a bg:#81c784 bold)\
([ {battery}% ](fg:#ffffff bg:#3b4252))\
"""
```

```text
set -g status-right '#(stfg right_tmux)'
```

- Plain group: values are joined as usual, with every `#` in values and `separator` doubled.
- Format group: spans become `#[fg=..,bg=..,bold]` ... `#[default]`; nested spans re-emit the outer
  style after the inner one closes. `#[default]` restores tmux's `status-style` (colours and
  attributes), not the terminal default. Literal template text and badge values have `#`
  doubled. The tmux dialect adds no control bytes, but ESC or other control bytes inside badge
  values pass through (tmux shows them literally).
- Colors: `#rrggbb` stays, 0-255 becomes `colourN`, `purple` becomes `magenta`, `bright-x`
  becomes `brightx`; palette names resolve to these. Modifiers map to `bold dim italics
  underscore blink reverse hidden strikethrough`.

Same hot-path client, no daemon-start wait.

### Limitations & workarounds

- One group cannot serve both starship and tmux (`output` is per group). Define two groups, e.g.
  `right` and `right_tmux`, with the same badges; palette names keep their colors consistent.
- Plain `#(stfg <badge>)` (a single badge, no group) does **not** escape `#`. A value containing
  `#` (e.g. from an http or command badge) can break or inject into the status line. Wrap it in
  a one-member tmux group instead.
- tmux refreshes `#()` once per `status-interval` and shows the previous output meanwhile; the
  first render is empty.
- tmux uses only one output line, so a multi-badge `stfg a b` is wrong there; use a group.

## Batching calls

Every `stfg` invocation is a process spawn (about 0.4–0.65 ms measured); the IPC and daemon
work is only ~0.14 ms of a ~0.7 ms call. So processes per prompt is the main cost. Starship runs
custom modules in parallel, so the saving is mostly CPU, plus less daemon work: one request
means one git repo discovery and `.git` read instead of one per badge.

Rules of thumb:

1. Adjacent badges with the same style: one plain [group](#groups) (`badges` + `separator`) in
   one module.
2. Badges spanning powerline segments or different colors: one [styled group](#styled-groups)
   (`format`), used with `format = '$output'` in starship.
3. tmux: one `#(stfg <group>)` per status side, with the group set to `output = "tmux"`. `#()`
   shows only the first line, so a multi-badge `stfg a b` (multi-line) is wrong there; use a
   group.
4. Group by prompt position: badges separated by non-star-forge starship modules (`$fill`,
   `$kubernetes`, ...) need separate groups, so the realistic minimum is one call per
   contiguous run.
5. Path-scoped git badges and global badges can share a group (cwd is sent once per call).

Before/after: 9 per-badge modules (7 git badges + `public_ip` + `battery`) become 2 modules,
the `git` group and the `right` styled group from the example above: 9 `stfg` calls → 2.

Verify with `hyperfine -N 'stfg git'` and by counting your `[custom.*]` modules; `stfgd status`
shows the configured badges.

## Filesystem isolation

Git discovery/file reads, host builtins, configuration reads, process spawning, and
extraction run on a bounded blocking pool, not on the daemon's single-threaded async
event loop. Global-only requests do not inspect the client's repository. Request-time git reads
and cold fetch waits share a budget of at least 20 ms (or `cold_wait` if larger), inside the
client's 40 ms deadline; configuration/power maintenance has a 500 ms budget and implicit checks
run in the background.

A filesystem syscall already blocked on NFS/FUSE cannot be cancelled by a timeout.
The daemon stops waiting, but its worker slot stays occupied until the operation finishes.
There are at most four admitted blocking jobs and no unbounded application queue.
If all slots are stuck, new filesystem-dependent work fails softly; cached global values
and administrative commands remain available. `stop` does not wait for stuck workers.
Restart the daemon after fixing the mount if its worker capacity does not recover.

The runtime directory should be on a healthy local filesystem: startup must create/lock
the socket directory before it can serve requests.

The client bounds its daemon launch with a worker thread under the same 40 ms deadline. A
stuck syscall is abandoned, not cancelled; the client exits without joining it. The implicit
`current_dir()` lookup (skipped with `--cwd`) runs inline on Linux, where `getcwd(2)` reads
the dentry cache, not the filesystem, so it can't hang on a dead mount; on macOS it runs on a
deadline-bounded worker as well.

## Environment variables

- `STAR_FORGE_CONFIG` — config file path override.
- `STAR_FORGE_TIMEOUT_MS` — override the client's 40 ms deadline (mostly for testing).
- `XDG_RUNTIME_DIR` / `XDG_CONFIG_HOME` — standard XDG dirs; fall back to `/tmp/star-forge-$UID`
  (the usual case on macOS, which doesn't set `XDG_RUNTIME_DIR`) and `~/.config` respectively.
  The runtime dir must be a real directory owned by you and not group/other-writable; anything
  else (e.g. a symlink planted in `/tmp`) makes `get` print empty lines and never start a daemon.

## Acknowledgements

The idea behind star-forge — one daemon that computes statusline values once and serves every
prompt/tmux call from its cache — comes from
[beachcomber](https://github.com/NavistAu/beachcomber) (MIT, Copyright (c) 2026 Joshua
Hogendorn). Parts of the in-process git readers in `src/provider/git.rs` are also adapted from it.
See [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) for the full list and license text.
