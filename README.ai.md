# star-forge setup guide for AI agents

Instructions for an AI coding agent that sets up star-forge for a user. A person who wants to
read about star-forge should start with [`README.md`](README.md); the full option reference is
[`docs/configuration.md`](docs/configuration.md). Contributors to this repository should read
[`AGENTS.md`](AGENTS.md) instead.

## What you are setting up

- `stfgd` is a per-user daemon (it starts itself on first use and exits after `idle_exit`) plus
  admin commands: `status`, `stop`, `reload`.
- `stfg <name>...` is the hot-path client. It prints one line per name (a badge or group), always
  exits 0, never writes to stderr, and prints an empty line for anything unknown, failing, or not
  yet fetched.
- Badges are defined in `~/.config/star-forge/config.toml` (or `$XDG_CONFIG_HOME/star-forge/`,
  or `$STAR_FORGE_CONFIG`). A missing file means no badges.
- starship, tmux or any script shows a badge by running `stfg <name>`.

The goal is usually to move slow starship `[custom.*]` modules (network calls, slow CLIs) and
many-module git segments behind `stfg`, so the prompt never waits.

## Ground rules

1. **Ask before you change anything outside the star-forge config.** Describe the plan
   (badges, which starship modules change) and get the user's agreement first.
2. **Back up every file before editing it**, e.g. `cp ~/.config/starship.toml
   ~/.config/starship.toml.bak-$(date +%s)`. Edit in place; don't regenerate whole files.
3. **Don't invent config keys.** An unknown key in `[daemon]`, `[badge.*]` or `[groups.*]`
   rejects the whole config file, and a misspelled table name (`[badges.x]`) is silently
   ignored. Use only the keys in [the schema](#config-schema) below.
4. **Keep secrets out of configs you print.** `http` badges may carry `headers` with tokens. Never
   echo their values back to the user or into logs.
5. **Don't change what the prompt shows** unless the user asked for it. A migrated module
   should look the same as before: same icons, colors and conditions.
6. Use `sudo` only for the package-manager installs below, and only after the user agrees.

## Step 1: Inspect the environment

Collect these before choosing anything:

```sh
uname -sm                                  # Linux/Darwin, x86_64/arm64
command -v stfg stfgd starship tmux mise packslip nix cargo dnf apt-get
starship --version
echo "${STARSHIP_CONFIG:-$HOME/.config/starship.toml}"
ls -l "${XDG_CONFIG_HOME:-$HOME/.config}/star-forge/config.toml"
```

If `stfg` and `stfgd` already exist, skip step 2. Neither binary has a `--version` flag.

## Step 2: Install

Supported: Linux `x86_64` (glibc or musl) and macOS (`aarch64`, `x86_64`). Pick the first
method that applies and that the user is happy with:

| Condition | Command |
| --- | --- |
| `mise` present | `mise use -g github:SkeLLLa/star-forge` |
| `packslip` present | `packslip install github.com/SkeLLLa/star-forge --pin ps1_iqg6ch676syauz3nb3hluddyku` |
| Nix with flakes | `nix profile install github:SkeLLLa/star-forge` |
| Fedora / RPM (`dnf`) | add the DNF repository from [README.md](README.md#rpm-and-apt-repositories), then `sudo dnf install star-forge` |
| Debian / Ubuntu (`apt-get`) | add the APT repository from [README.md](README.md#rpm-and-apt-repositories), then `sudo apt install star-forge` |
| Rust toolchain | `cargo install star-forge` |

- Always install the latest release. Don't pin a version unless the user asks for one.
- The packslip `--pin` value identifies the signer, not a release; it is the same for every
  version.
- After installing, confirm with `command -v stfg stfgd`. If they aren't found, the install's
  bin directory (`~/.local/bin`, `~/.cargo/bin`, the mise shims) isn't on `PATH`; tell the user
  rather than editing their shell profile unasked.

## Step 3: Read the existing prompt config

Read the starship config and list:

- Every `[custom.*]` module: its `command`, `shell`, `when`, `detect_*`, `format`, `style`, and
  where it appears in the top-level `format` / `right_format`.
- Built-in modules the user may want cached: `$git_branch`, `$git_status`, `$git_commit`,
  `$git_state`, `$nodejs`, `$python`, `$rust`, `$golang`, `$ruby`.
- `palette` and `[palettes.*]`: star-forge can reuse them.
- Runs of adjacent modules: each run can become one group (one `stfg` call).

Good candidates are commands that make network requests, spawn slow tools, or take more than a
few milliseconds. Fast built-in modules that aren't in a run you're grouping can stay as they
are.

## Step 4: Plan with the user

Propose a short table (starship module → badge or group name → type) and get approval.
Naming convention: badge `public_ip`, starship module `[custom.sf_public_ip]`. Group and badge
names share one namespace and must not collide.

## Step 5: Write the star-forge config

Create or extend `~/.config/star-forge/config.toml`.

**A starship custom command becomes a `command` badge:**

```toml
# before (starship.toml)
# [custom.weather]
# command = "curl -sf 'https://wttr.in/?format=%t'"
# when = true

[badge.weather]
type = "command"
command = "curl -sf 'https://wttr.in/?format=%t'"   # no `args` => run via `sh -c`
interval = "30m"     # how often to refresh while in use
timeout = "5s"       # hard kill; default 2s
```

- Choose `interval` by how fast the value changes: seconds for git counts, minutes for network
  data, hours for things like kernel version.
- If the command depends on the current directory, add `scope = "git_root"` (runs at the repo
  root, cached per repo) or `scope = "project_root"` with `markers = [...]`.
- If it depends on the caller's environment (`PATH`, version managers), add
  `env = ["PATH", "MISE_*"]`.
- If starship used `detect_files` to show it only in some projects, use a path scope with
  `when_file = ["package.json"]`.

**Use an `http` badge** when the command is just `curl` plus JSON parsing:

```toml
[badge.public_ip]
type = "http"
url = "https://api.ipify.org?format=json"
extract = { kind = "json", pointer = "/ip" }
interval = "10m"
```

**Git and system values have builtins.** Prefer them over shelling out:

```toml
[badge.git_branch]
type = "builtin"
name = "git_branch"
format = " {value}"

[badge.git_ahead]
type = "builtin"
name = "git_counts"
field = "ahead"
format = "⇡{value}"     # zero counts render empty, so the format disappears too
```

**Group adjacent badges** to cut process spawns. Plain:

```toml
[groups.git]
badges = ["git_branch", "git_ahead"]
separator = " "
```

Or styled, copying the user's starship segment and replacing each `${custom.sf_x}` with `{x}`:

```toml
[daemon]
palette_from = "~/.config/starship.toml"   # reuse the starship palette names

[groups.git]
format = """
([  {git_branch} ](bg:color_bg_l2 fg:color_fg_l2))\
([{git_ahead} ](bg:color_bg_l2 fg:color_fg_l2))\
"""
```

- `( ... )` drops its whole contents when every badge inside is empty.
- In TOML basic strings a trailing `\` joins lines (wanted). A literal `\[` must be written
  `\\[`, or use `'''...'''`.

## Step 6: Wire it into starship

Use exactly this shape for every star-forge module:

```toml
[custom.sf_public_ip]
command = "public_ip"          # badge or group name, passed as argv to stfg
shell = ["stfg"]               # stfg itself is the "shell": no `sh -c` fork
use_stdin = false
when = true                    # required: custom modules are hidden without when/detect_*
format = "([$output ]($style))"
style = "bg:color_bg_l1 fg:color_fg_l1"
```

- For a styled group use `format = '$output'` and no `style`; the group carries its own
  styling.
- Replace the old module's `${custom.<old>}` reference in `format` with `${custom.sf_<name>}`,
  and remove the old `[custom.<old>]` table only after verifying the new one works.
- If the starship log shows `Scanning current directory timed out` (common on the first prompt
  after a reboot), add `scan_timeout = 100` at the top level of `starship.toml`, before any
  table. That timeout is starship's own directory scan and is unrelated to star-forge.

## Step 7: Verify

```sh
stfg public_ip; sleep 1; stfg public_ip   # first call may be empty (cold); second prints the value
stfgd reload                              # apply config edits now; prints "reload ok" either way
stfgd status                              # badge, scope, age_s, errors, then any "config error:"
starship prompt                           # render the prompt once from the shell
```

- **A `config error:` line in `stfgd status`** means the whole file was rejected. Fix it and
  run `stfgd reload` again. `stfgd reload` reports `reload ok` even when the new config is
  rejected, so always check `status` afterwards.
- **A non-zero `errors` count** means the command or request failed. Run the badge's `command`
  by hand to see why.
- **`age_s` of `-`** means the badge has no value yet.
- **The badge isn't listed at all**: check the name, and that the file is the path from step 1.
- `stfgd status` exits 3 when no daemon is running; any `stfg` call starts one.
- After a successful check, delete the old starship modules the user agreed to replace, and
  tell the user where the backups are.

## Step 8: Optional extras

- **tmux:** define a group with `output = "tmux"` (tmux can't render ANSI) and set
  `set -g status-right '#(stfg right_tmux)'`. A group can't serve both starship and tmux; define
  two with the same badges.
- **Start with the session (Linux packages only):** `systemctl --user enable --now
  star-forge.service`. For other installs see [`docs/distribution.md`](docs/distribution.md).
- **On battery:** set `battery_interval` (daemon-wide or per badge) to refresh less often.

## Config schema

Durations are strings `<integer><unit>`, unit `ms|s|m|h`. The only top-level tables are
`[daemon]`, `[palette]`, `[badge.<name>]` and `[groups.<name>]`.

`[daemon]` (all optional): `idle_exit` (30m), `interval` (60s), `battery_interval`,
`active_window` (5m), `coalesce` (1s, must be < every interval), `retry_min` (2s), `retry_max`
(5m), `timeout` (2s), `power_check` (60s), `config_check` (2s), `cold_wait` (20ms, must be
< 30ms), `path_evict` (30m), `max_paths` (256), `max_output` (65536), `palette_from`.

`[palette]`: inline `name = "#rrggbb"` colors; overrides `palette_from`.

`[badge.<name>]`:

| Key | Applies to | Notes |
| --- | --- | --- |
| `type` | all | `builtin`, `command`, or `http` |
| `format` | all | default `{value}`; `{value}` is the only placeholder |
| `interval`, `battery_interval`, `active_window`, `timeout`, `max_output` | all | fall back to `[daemon]` |
| `extract` | all | `{ kind = "trim" }` (default), `{ kind = "regex", pattern = "...", group = N }`, `{ kind = "json", pointer = "/a/b" }` |
| `scope` | see below | `global`, `git_root`, `project_root` |
| `command`, `args` | `command`, `tool_version` | without `args`, `command` runs via `sh -c` |
| `url`, `headers` | `http` | |
| `name` | `builtin` | see the builtins below |
| `field` | `git_counts` only | `ahead`, `behind`, `staged`, `modified`, `untracked`, `conflicted` |
| `device` | `battery` only | e.g. `BAT0` (Linux), `InternalBattery-0` (macOS) |
| `tool` | `tool_version` only | `node`, `python`, `rust`, `go`, `ruby` |
| `markers` | `command`, `tool_version` | required with, and only valid for, `project_root` |
| `watch`, `when_file` | `command`, `tool_version` | path scopes only; paths relative to the scope root |
| `env` | `command`, `tool_version` | variable names from the caller; trailing `*` is a prefix |

Builtins:

- **Git:** `git_branch`, `git_commit`, `git_state`, `git_stash`, `git_status`, `git_counts`.
  They must use `scope = "git_root"` (their default; `global` is rejected) and render empty
  outside a repo.
- **Host-wide:** `battery` (percent, no `%`), `hostname`, `load_avg`, `mem_used_percent`,
  `uptime` (seconds).
- **`tool_version`:** the project's tool version, e.g. `22.11.0`. Empty outside matching
  projects.

Scope: `command` badges default to `global` and may use any scope. `http` and host builtins are
`global` or `git_root`.

`[groups.<name>]`: either `badges = [...]` (non-empty, existing badges only, no nesting) with
optional `separator` (default `" "`), or `format = "..."` (starship format syntax, mutually
exclusive with `badges`/`separator`). Optional `output = "tmux"`.

For anything not covered here, read [`docs/configuration.md`](docs/configuration.md).
