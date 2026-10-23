# star-forge

![star-forge logo](docs/assets/logo.svg)

[![CI](https://github.com/SkeLLLa/star-forge/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/SkeLLLa/star-forge/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SkeLLLa/star-forge)](https://github.com/SkeLLLa/star-forge/releases/latest)
[![crates.io](https://img.shields.io/crates/v/star-forge)](https://crates.io/crates/star-forge)
[![MSRV](https://img.shields.io/crates/msrv/star-forge)](Cargo.toml)
[![License: GPL-3.0-or-later](https://img.shields.io/crates/l/star-forge)](COPYING)

Fast, cached badges for your shell prompt and tmux status line.

[Starship](https://starship.rs) redraws your prompt from scratch every time. Its built-in
modules are quick, but a `[custom.*]` module (your public IP, the weather, CI status, a slow
CLI) runs again on every Enter. A 300 ms `curl` means a 300 ms pause before every prompt.

star-forge moves that work into a small background daemon that refreshes each value on its own
schedule. Your prompt reads the cached value with `stfg`, which usually answers in under a
millisecond and never takes longer than 30 ms.

![Starship prompt with public IP and weather badges cached by star-forge](docs/assets/prompt-demo.png)

> **Setting up with an AI assistant?** Point it at [`README.ai.md`](README.ai.md). It
> contains step-by-step instructions written for agents.

## What you get

- **A prompt that never waits.** Slow commands refresh in the background; the prompt shows
  the last known value right away.
- **No error noise.** If something fails, the badge is just empty. Nothing is printed to your
  terminal.
- **One run for every shell.** Ten terminals and a tmux bar share one cache, so a weather API
  gets one request per interval, not one per prompt.
- **Cheap git info.** Branch, commit, rebase state and stash count are read straight from
  `.git`. Counts that need `git status` are cached and refreshed when you stage, commit or
  switch branches.
- **Fewer processes.** A group renders several badges, styled with starship's own syntax and
  palette, from one `stfg` call.
- **Easy on your battery.** Badges refresh only while in use, and the daemon quits after 30 idle
  minutes. It starts again by itself when needed.
- **tmux too.** The same badges render in tmux's `#[fg=...]` syntax.

If your prompt only uses starship's built-in modules and already feels instant, you don't need
star-forge.

## Install

star-forge runs on Linux and macOS. You get two binaries: `stfg`, the tiny client your prompt
calls, and `stfgd`, the daemon and admin CLI. There is nothing to start: the daemon launches
itself on first use.

| Platform | How to install |
| --- | --- |
| Fedora and other DNF-based systems | [DNF repository](#fedora-dnf-repository), [.rpm](#a-single-deb-or-rpm-file), [tarball](#any-platform-tarball) |
| Debian, Ubuntu and other APT-based systems | [APT repository](#debian-and-ubuntu-apt-repository), [.deb](#a-single-deb-or-rpm-file), [tarball](#any-platform-tarball) |
| macOS | [mise, packslip, Nix or cargo](#mise-packslip-nix-or-cargo), [tarball](#any-platform-tarball) |
| Any platform | [mise, packslip, Nix or cargo](#mise-packslip-nix-or-cargo), [tarball](#any-platform-tarball) |

All downloads are on the [latest release](https://github.com/SkeLLLa/star-forge/releases/latest)
page. Native packages and repositories are x86_64 only.

### Fedora: DNF repository

This is the recommended method on Fedora: `dnf upgrade` then keeps `star-forge` up to date.

```sh
sudo curl -fsSLo /etc/yum.repos.d/star-forge.repo https://skellla.github.io/star-forge/rpm/star-forge.repo
sudo dnf install star-forge
```

### Debian and Ubuntu: APT repository

This is the recommended method on Debian and Ubuntu: `apt upgrade` then keeps `star-forge` up to
date.

```sh
echo 'deb [trusted=yes] https://skellla.github.io/star-forge/deb stable main' | sudo tee /etc/apt/sources.list.d/star-forge.list
sudo apt update
sudo apt install star-forge
```

The repositories are unsigned (`trusted=yes`, `repo_gpgcheck=0`), so their integrity depends on
HTTPS to GitHub Pages. They track every release.

### A single .deb or .rpm file

You get no automatic updates with this method. Download the file from the
[latest release](https://github.com/SkeLLLa/star-forge/releases/latest), then run:

```sh
sudo apt install ./star-forge_<version>_amd64.deb      # Debian, Ubuntu
sudo dnf install ./star-forge-<version>-1.x86_64.rpm   # Fedora
```

Packages install `/usr/bin/stfg`, `/usr/bin/stfgd` and the user service
`/usr/lib/systemd/user/star-forge.service`.

### Any platform: tarball

Tarballs exist for Linux (`x86_64` glibc and static musl) and macOS (`aarch64`, `x86_64`). This
method installs for your user only and doesn't need root:

```sh
tar -xzf star-forge-<version>-<target>.tar.gz
install -m 755 star-forge-<version>-<target>/bin/* ~/.local/bin/
```

Each tarball has a `.sha256` sidecar, and the release has a combined `SHA256SUMS`. Pick the musl
tarball for the fastest `stfg` startup or on a distro without a compatible glibc.

### mise, packslip, Nix or cargo

These put `stfg` and `stfgd` on your `PATH`. Pick one:

```sh
# mise. Append @<version> to pin a release.
mise use -g github:SkeLLLa/star-forge

# packslip. Checks the signed release manifest before it installs into ~/.local/bin.
packslip install github.com/SkeLLLa/star-forge --pin ps1_iqg6ch676syauz3nb3hluddyku

# Nix. Builds from source; on Linux it also installs the user service.
nix profile install github:SkeLLLa/star-forge

# cargo. Builds from crates.io.
cargo install star-forge --locked
```

The packslip `--pin` fingerprint identifies this repository's release workflow and stays the same
across releases. Check it against this README rather than trusting it on first use. With mise,
`mise use -g packslip:github.com/SkeLLLa/star-forge` also checks the signed manifest.

### After installing (optional)

The daemon starts on the first prompt and exits after `daemon.idle_exit` (30 minutes by default)
of inactivity, so no service is needed. On Linux you can start it with your session instead. The
packages and Nix ship the unit; for other methods see
[`docs/distribution.md`](docs/distribution.md#cargo-install) for a unit to copy.

```sh
systemctl --user daemon-reload
systemctl --user enable --now star-forge.service
```

Next, see [Quick start](#quick-start).

### Uninstall

First run `stfgd stop` (or `systemctl --user disable --now star-forge.service` if you enabled the
unit), then:

| Installed with | Remove with |
| --- | --- |
| DNF | `sudo dnf remove star-forge`, then `sudo rm /etc/yum.repos.d/star-forge.repo` |
| APT | `sudo apt remove star-forge`, then `sudo rm /etc/apt/sources.list.d/star-forge.list` |
| Tarball or packslip | `rm ~/.local/bin/stfg ~/.local/bin/stfgd` |
| mise | `mise unuse -g github:SkeLLLa/star-forge` |
| Nix | `nix profile remove star-forge` |
| cargo | `cargo uninstall star-forge` |

Your config in `~/.config/star-forge` stays in place until you delete it.

## Quick start

1. Describe a badge in `~/.config/star-forge/config.toml`:

   ```toml
   [badge.public_ip]
   type = "http"
   url = "https://api.ipify.org?format=json"
   extract = { kind = "json", pointer = "/ip" }
   interval = "10m"
   ```

2. Show it in `~/.config/starship.toml`:

   ```toml
   [custom.sf_public_ip]
   command = "public_ip"
   shell = ["stfg"]
   use_stdin = false
   when = true
   format = "([$output ]($style))"
   ```

3. Open a new prompt. The very first one may be empty while the value is fetched; after that
   it's always there. Run `stfgd status` to see what's cached.

Config changes are picked up automatically within a couple of seconds.

## Common recipes

**Run any command** (pipes work; it runs through `sh -c`):

```toml
[badge.weather]
type = "command"
command = "curl -sf 'https://wttr.in/?format=%t'"
interval = "30m"
timeout = "5s"
```

**Git info**, computed without spawning `git` where possible:

```toml
[badge.git_branch]
type = "builtin"
name = "git_branch"
format = " {value}"

[badge.git_modified]
type = "builtin"
name = "git_counts"
field = "modified"    # also: ahead, behind, staged, untracked, conflicted
format = "!{value}"
```

**Language versions** for the current project, shown only where they apply:

```toml
[badge.node]
type = "builtin"
name = "tool_version"
tool = "node"         # also: python, rust, go, ruby
format = " {value}"
```

**Several badges in one module.** Each `stfg` call is a process, so group badges that sit next
to each other:

```toml
[groups.git]
badges = ["git_branch", "git_modified"]
separator = " "
```

Then use `command = "git"` in a single starship `[custom.*]` module. Groups can also carry full
starship styling and your starship palette, which lets one module replace a whole powerline
segment; see [styled groups](docs/configuration.md#styled-groups).

**tmux:** add a group with `output = "tmux"` and put `#(stfg <group>)` in `status-right`. See
[tmux](docs/configuration.md#tmux).

Other builtins: `battery`, `hostname`, `load_avg`, `mem_used_percent`, `uptime`, `git_commit`,
`git_state`, `git_stash`, `git_status`.

## Benchmarks

`stfg` with a warm cache against running the same work directly, as a starship `[custom.*]`
module would. Measured with [hyperfine](https://github.com/sharkdp/hyperfine) on an Intel Core
Ultra 7 165H (Linux, release build), in the star-forge repository itself:

| Work | `stfg` | Bare command | Speedup |
| --- | ---: | ---: | ---: |
| Branch name (`git branch --show-current`) | 0.79 ms | 1.06 ms | 1.3× |
| Modified count (`git status --porcelain=v2 --branch`) | 0.78 ms | 1.63 ms | 2.1× |
| 300 ms command (`sh -c 'sleep 0.3; echo ok'`) | 0.76 ms | 302.7 ms | 399× |
| All three in one group vs. all three commands | 0.80 ms | 305.9 ms | 380× |

Most of an `stfg` call is starting the process; the daemon's share is about 0.14 ms, measured
separately (see [batching calls](docs/configuration.md#batching-calls)). The `sleep` stands in
for a network call or slow CLI, so it shows the real difference. `git status` gets slower as
the repository grows, but a cached `stfg` read doesn't run it: the daemon refreshes it in the
background, so unstaged edits can show up to one `interval` late. A bare command also blocks
the prompt for its whole run, while `stfg` waits at most 30 ms for the daemon.

The last row adds up the work. Starship runs `[custom.*]` modules in parallel, so the prompt
delay you would actually see from the bare commands is that of the slowest one (here the
300 ms command), not their sum.

Run it on your machine with `mise run bench` from a checkout. It uses a throwaway config and
daemon, so your own setup is untouched. Set `SLOW_SECS` to change the stand-in delay. Results
are also written to `bench/results.md` under Cargo's target directory.

## Development

```sh
mise install          # Rust toolchain, mr-boxington, linters
mise run build        # debug build; `mise run build-release` for target/release/{stfg,stfgd}
mise run check        # fmt-check, clippy, tests, docs, Markdown lint
```

The mise tasks run Cargo through [Mr Boxington](https://mr-boxington.jdx.dev) (`mbx`), which
reuses compiled Rust work across builds and checkouts on this machine. The first build fills the
cache; later matching builds reuse it. Plain `cargo` still works and skips the cache, which is
what CI uses. Run `mise tasks` for the full list.

## Troubleshooting

- **A badge is empty.** Run `stfgd status`: it lists every badge with its age and last error,
  plus the last config error. A config with an error is rejected as a whole: the daemon keeps the
  previous one (or none) until it's fixed.
- **`Scanning current directory timed out` in starship's log**, usually on the first prompt
  after a reboot. This is starship's own directory scan, not star-forge. Raise it at the top of
  `starship.toml`: `scan_timeout = 100`.
- **Restart the daemon:** `stfgd stop`; the next prompt starts it again.

## Learn more

- [`docs/configuration.md`](docs/configuration.md): every option, badge type and integration.
- [`docs/distribution.md`](docs/distribution.md): packages, systemd and the release pipeline.
- [`docs/design.md`](docs/design.md): how it works inside.

## Acknowledgements

The idea behind star-forge (one daemon that computes statusline values once and serves every
prompt and tmux call from its cache) comes from
[beachcomber](https://github.com/NavistAu/beachcomber) (MIT, Copyright (c) 2026 Joshua
Hogendorn). Parts of the in-process git readers in `src/provider/git.rs` are also adapted from it.
See [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) for the full list and license text.

## Support

If `star-forge` is useful to you and you want to say thanks, please consider supporting Ukrainian
defenders instead of sending money to the author.

[![Come Back Alive](docs/assets/badges/donate-come-back-alive.svg)](https://savelife.in.ua/en/donate-en/)
[![Sternenko Fund](docs/assets/badges/donate-sternenko-fund.svg)](https://www.sternenkofund.org/en/donate)
[![Prytula Foundation](docs/assets/badges/donate-prytula-foundation.svg)](https://prytulafoundation.org/en/donation)
