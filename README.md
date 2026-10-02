# star-forge

A small daemon plus a synchronous CLI client that cache statusline values ("badges") so
prompt tools (starship `[custom.*]`, tmux, …) render instantly. Values are refreshed in the
background; rendering never waits on a provider.

- `stfg <badge>...` (the tiny hot-path client; `stfgd get <badge>...` is the exact
  equivalent) always exits 0 and prints one line per badge (empty on any failure), bounded
  by a 40 ms deadline. It never writes to stderr and never waits for the daemon to start.
- The daemon refreshes badges on demand, caches per badge (globally, or per git repo root
  for git-scoped builtins), and exits after 30 minutes of inactivity by default. The next
  `get` respawns it.

See `docs/design.md` for the full architecture.

## Install

```sh
cargo install star-forge        # or: cargo install --path .
```

Each GitHub release also ships Linux (`x86_64`) and macOS (`aarch64`, `x86_64`) tarballs plus
`.deb`/`.rpm` packages. The packages are also published as APT/DNF repositories on GitHub Pages
(<https://skellla.github.io/star-forge>).

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
stfgd status                          # table of cached badges, scope, age, errors
stfgd stop
stfgd reload
```

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
retry_min = "2s"           # default 2s: backoff after a failed fetch starts here, doubling on each failure
retry_max = "5m"           # default 5m: backoff cap
timeout = "2s"             # default 2s: fallback hard kill for the whole fetch + extract
power_check = "60s"        # default 60s: how often AC/battery state is re-checked
config_check = "2s"        # default 2s: how often the config file's mtime is polled for an implicit reload
cold_wait = "20ms"         # default 20ms: how long a cold (no value yet) `get` waits for its own fetch; must be < 40ms
path_evict = "30m"         # default 30m: idle git-scoped badges are dropped after this long
max_paths = 256            # default 256: LRU cap on git-scoped (per-repo) cache entries
max_output = 65536         # default 64 KiB: fallback cap on command stdout / HTTP body

[badge.<name>]
type = "builtin" | "command" | "http"
format = "{value}"             # default "{value}"; the one placeholder is replaced with the raw value
interval = "5s"                 # optional; defaults to [daemon] interval. Refresh period (global) / freshness TTL (git-scoped)
battery_interval = "5s"         # optional; resolution order below
active_window = "5m"            # optional; defaults to [daemon] active_window
timeout = "2s"                  # optional; defaults to [daemon] timeout
max_output = 65536              # optional; defaults to [daemon] max_output
scope = "git_root" | "global"   # optional; defaults to git_root for git builtins, global otherwise
extract = { kind = "trim" | "regex" | "json", ... }   # default: trim
```

Every badge-level knob above falls back to its `[daemon]` counterpart when unset, except
`battery_interval`, which has its own four-level chain: badge `battery_interval` → badge
`interval` (if set) → daemon `battery_interval` → daemon `interval`. Unknown fields in
`[daemon]` or any `[badge.*]` are rejected at load.

`type = "builtin"` badges (`name = "git_branch" | "git_status" | "git_counts" | "git_commit" |
"git_state" | "git_stash" | "battery" | "hostname" | "load_avg" | "mem_used_percent" |
"uptime"`):

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

All six git builtins require `scope = "git_root"` (also their default): they use the repo
containing `--cwd`, and render empty outside a repo without running a subprocess.
`scope = "global"` on a git builtin is rejected at config load rather than silently ignored.
Everything else defaults to global and can override this with
`scope = "git_root" | "global"`. A `command` badge scoped to `git_root` is run with
cwd = the repo root and cached per repo, same as a git builtin.

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
One `[custom.*]` block per badge; verified against starship 1.26.

## tmux

```text
#(stfg battery)
```

Same hot-path client, no daemon-start wait: empty on a cold cache, filled in on the next
render once the background refresh lands.

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
Hogendorn). Parts of the in-process git readers in `src/provider.rs` are also adapted from it.
See [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) for the full list and license text.
