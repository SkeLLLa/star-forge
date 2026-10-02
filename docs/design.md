# star-forge — Architecture Design

A small daemon plus a synchronous CLI client that cache statusline values ("badges") so prompt tools
(starship `[custom.*]`, tmux, …) render instantly. Values are refreshed in the background, and
rendering never waits on a provider.

The core idea — one daemon computes these values once and every consumer reads them from its
cache over a Unix socket — comes from [beachcomber](https://github.com/NavistAu/beachcomber).

Packaging, the systemd user unit, release CI, and the GitHub Pages APT/DNF repositories are
described in [`distribution.md`](distribution.md).

## 0. Decisions

| Question | Decision |
|---|---|
| IPC | Unix domain socket; admin (`status`/`stop`/`reload`) is one NDJSON request/response per connection, `get` is a plain-text line protocol (see §2) |
| Client | plain `std` + `libc` only (no tokio runtime, no serde), hard 40 ms total deadline, always exit 0 for `get` |
| Daemon runtime | tokio `current_thread` |
| Singleton | `flock(LOCK_EX\|LOCK_NB)` on a lock file held for process lifetime; no pidfile |
| HTTP | `reqwest`, `default-features = false`, `rustls` |
| CLI parsing | hand-rolled (5 subcommands, 1 flag) |
| Subprocess kill | `process_group(0)` + `killpg(SIGKILL)` on timeout, always reaped |
| Platforms | Linux and macOS (aarch64, x86_64); `cfg(target_os)` picks the host-builtin backend, everything else is shared |
| Builtins | `git_branch`, `git_status`, `git_counts` (`git status --porcelain=v2`, deduped per repo); `git_commit`/`git_state`/`git_stash` (in-process, no subprocess); `battery` (Linux `/sys/class/power_supply`; macOS `pmset -g batt` subprocess, bounded by `timeout`); `hostname` (`gethostname(3)`); `load_avg` (`getloadavg(3)`); `uptime` (Linux `CLOCK_BOOTTIME`; macOS `kern.boottime`); `mem_used_percent` (Linux `/proc/meminfo`; macOS `host_statistics64` + `hw.memsize`) |
| Git freshness | `git_branch`/`git_commit`/`git_state`/`git_stash` computed in-process per request (no cache, no stale window); `git_status`/`git_counts` invalidate their cache entry on a `.git/HEAD`/`.git/index` mtime change instead of waiting out `interval` (see §5/§6) |
| Cache scope | explicit `Scope { Global, GitRoot }` on every badge; git builtins require `GitRoot`, other sources can override their default |
| Scheduling | demand-driven, one coalesced timer, dormant when unused (see §6) |

## 1. Process model & IPC

Socket: `$XDG_RUNTIME_DIR/star-forge/sock` (fallback `/tmp/star-forge-$UID/sock`, dir mode 0700).
Lock: same dir, `daemon.lock`. Config: `$XDG_CONFIG_HOME/star-forge/config.toml`
(fallback `~/.config/star-forge/config.toml`), overridable by `STAR_FORGE_CONFIG`.
macOS doesn't set `XDG_RUNTIME_DIR`, so the `/tmp` fallback is the default there. Since `/tmp` is
shared, the daemon creates the dir with mode 0700 and exits unless the path is a real directory
(not a symlink) owned by `getuid()` — another user squatting `/tmp/star-forge-$UID` can't capture
the socket.

Files written by the daemon were considered and rejected: path-scoped badges need the client's cwd at
query time, and the daemon needs query activity to drive demand-based scheduling.

**Client `get` path** (sync, `std::os::unix::net::UnixStream`, implemented once in `src/client.rs`
and shared by both binaries — see §3):

1. Compute a deadline `now + 40ms` (`STAR_FORGE_TIMEOUT_MS` override).
2. `lstat()` the runtime dir first: missing → spawn without connecting; anything but a real
   directory (not a symlink) owned by `getuid()` with no group/other write bits → print empty
   lines, exit 0, never spawn (`status`/`stop`/`reload` report "daemon not running"). Then
   connect via a non-blocking `socket(2)`/`connect(2)` (not `UnixStream::connect`): on Linux a
   blocking `connect()` to an `AF_UNIX` socket blocks the caller when the listener's backlog is
   full, which could blow through the 40 ms deadline; a non-blocking socket reports a full backlog
   as an immediate `EAGAIN` instead (no `EINPROGRESS` — `AF_UNIX` has no handshake to wait out).
   macOS reports a full backlog as `ECONNREFUSED`, so there it reads as "not running": the
   spawned daemon loses the `flock` race and exits — one wasted spawn under overload, still
   inside the deadline.
   `ENOENT`/`ECONNREFUSED` → spawn the daemon detached (below), print empty lines, exit 0. Any
   other error, including a full backlog → print empty lines, exit 0, **do not** spawn (a busy
   daemon doesn't need a second one racing it). On success, `O_NONBLOCK` is cleared and the fd is
   wrapped back into a `UnixStream` so the rest of the path is unchanged.
3. Enforce one absolute deadline across every write/read, recalculating the remaining budget
   before each operation; read the response through EOF with a 64 KiB cap. Trickle responses
   cannot reset the budget. Any error/timeout → print empty lines, exit 0. Nothing is ever
   written to stderr.

The detached daemon spawn runs on a std worker thread bounded by the same deadline; the client
waits for the spawn result only, never for daemon readiness. The implicit `current_dir()` lookup
(skipped when `--cwd` is given) runs inline on Linux, where `getcwd(2)` resolves from the dentry
cache without touching the filesystem, so a dead mount can't stall it. macOS makes no such
guarantee, so there it runs on a deadline-bounded worker too (timeout → all-empty output). A
timed-out syscall is not cancelled: the short-lived
client exits without joining its worker. stdout writes and OS scheduling are not real-time
guarantees, so the deadline bounds the client's own waits, not wall-clock process lifetime.

**Detached spawn**: the daemon binary is resolved as a sibling of the running executable first
(same directory as `current_exe()`, named `stfgd`), falling back to `stfgd` on `PATH` —
this lets the tiny `stfg` binary (which is not itself `stfgd`) start the daemon
without needing to *be* `stfgd`. `Command::new(resolved).arg("daemon")`, all stdio
`Stdio::null()`, not waited on, and deliberately **no** `process_group(0)` (a group leader cannot
`setsid()`; EPERM). The daemon itself calls `setsid()` first thing (new session + own group; drops
any controlling terminal, so SIGHUP from a closing terminal never reaches it) and `chdir("/")` (it
doesn't pin mount points). The client never unlinks the socket; the daemon does (see singleton), which
avoids a client deleting a live daemon's socket during a race.

**Singleton**: daemon opens `daemon.lock`, `flock(LOCK_EX|LOCK_NB)`. If that fails, another daemon
is live → exit 0. Once holding the lock it unlinks any stale socket and binds. The kernel releases the
lock on any death, so there is nothing stale to trust. On shutdown it unlinks the socket only if its
dev/ino still match the one it bound, so it never deletes a successor daemon's socket. It bumps
`daemon.lock`'s mtime at least once per `idle_exit`: macOS's daily periodic job deletes `/tmp`
files older than 3 days, and an unlinked lock would let a second daemon lock a fresh inode.

**Upgrade safety**: requests carry `"ver": env!("CARGO_PKG_VERSION")` (the client's version);
responses carry the daemon's. On a `get` whose `ver` differs from its own, the daemon answers
normally and only then initiates its normal graceful shutdown (killpg in-flight, unlink socket,
exit) — the same path `stop` uses. Zero extra client latency: there is no synchronous
notification round-trip on the hot path. The next prompt's connect finds no socket and spawns the
new binary.

## 2. Wire protocol

Two protocols share one socket, dispatched by the daemon on the first byte of the incoming line
(`{` → JSON, `g` → text): admin commands (`status`/`stop`/`reload`) stay JSON since they're off
the hot path and benefit from `serde`; `get` is a plain-text line protocol so the client never
needs to link `serde`/`serde_json`.

**Admin** (`cmd` ∈ `status | stop | reload`):

```jsonc
// request
{"v":1,"cmd":"status","ver":"0.1.0"}
// response
{"v":1,"ver":"0.1.0","ok":true,"values":{...}}
```

**`get`** — one line in, using `\u{1f}` (ASCII unit separator) as the field delimiter, one line
out per requested badge:

```text
get\x1f0.1.0\x1f/home/u/proj\x1fgit_branch\x1fgit_status\n
```

fields: literal `get`, the client's `CARGO_PKG_VERSION`, cwd, then one field per badge name.
If cwd or any badge name contains `\n`, `\r`, or `\x1f`, the client refuses to encode the request
at all and prints empty lines without connecting (see `client::encode_request`). The daemon
answers with exactly one line per requested badge, in the same order, then closes; a rendered
value's own `\n`/`\r` are replaced with a space so the line framing can't be broken by badge
content. Version mismatch (`ver` differs from the daemon's own `CARGO_PKG_VERSION`) → the daemon
answers normally, then initiates the same graceful shutdown `stop` uses (no added client latency;
see §1 upgrade safety). Both directions are capped at 64 KiB per line.

## 3. Module layout

- `src/main.rs` — CLI parsing and dispatch for the `stfgd` binary; `mod client;` for `get`.
- `src/client.rs` — the entire `get` hot path (std + `libc` only: socket path resolution,
  non-blocking connect, detached daemon spawn with sibling-binary/`PATH` lookup, the text wire
  protocol's request encoding/response parsing, single buffered stdout write). Compiled into
  `stfgd` via `main.rs`'s `mod client;`, and into `src/bin/stfg.rs` via
  `#[path = "../client.rs"] mod client;` — one implementation, two binaries, no shared lib target.
- `src/bin/stfg.rs` — the `stfg` binary: CLI parsing only, delegates
  everything to `client.rs`. Links only std + `libc` (no tokio/serde/serde_json/regex/reqwest/toml);
  see §9 for the size/latency payoff.
- `src/config.rs` — `Config`, `DaemonConfig`, `BadgeConfig`, `Builtin`, `Source`, `Scope`,
  `Extract`; `load(path)`. Deserializes into `RawDaemonConfig`/`RawBadgeConfig` first (every
  timing field an `Option<String>` duration, `deny_unknown_fields`), then resolves each
  against its `[daemon]`/fixed default and parses durations via `duration::parse` (see §4).
  `Scope` resolves to its default (`GitRoot` for path-scoped builtins, `Global` otherwise)
  unless a non-git source sets `scope = "..."` explicitly. Git builtins reject `Global`.
- `src/duration.rs` — hand-rolled `<integer><unit>` (`ms|s|m|h`) duration parser used for every
  timing knob; no derived formulas or multipliers anywhere in the crate.
- `src/ipc.rs` — daemon-side wire types, paths (re-exported from `client.rs`), bounded line
  readers, and the JSON admin (`status`/`stop`/`reload`) client. The `get` client lives entirely
  in `client.rs`; nothing here duplicates it.
- `src/daemon.rs` — setsid/chdir, flock, accept loop, request handling (dispatches JSON vs text
  `get` by first byte, see §2), reload/stop, idle exit. `handle_get` intercepts the four
  in-process git builtins before they ever touch the cache (see §5/§6), and applies
  request-time fingerprint invalidation for `git_status`/`git_counts`.
- `src/cache.rs` — `Key`, `Entry` (incl. the `git_status`/`git_counts` fingerprint, see §4),
  backoff, scheduling state (next-due computation, eviction), `effective_interval`
  (AC-aware; shared by the timer path and `handle_get`'s staleness check).
- `src/provider.rs` — `run_with_timeout`, HTTP fetch, builtins, git root discovery,
  in-process git file reads (`run_in_process_git`), `.git/HEAD`+`.git/index` fingerprinting
  (`git_fingerprint`), and in-flight `git_counts` sharing (`porcelain_fetch`, see §5).
- `src/blocking.rs` — four-slot admission for blocking work, absolute deadlines, and
  permits held until actual completion rather than released on waiter timeout.
- `src/extract.rs` — trim/regex/json extraction + `{value}` template rendering.

No provider plugin system: `Source` and `Extract` are closed enums dispatched with `match`.

## 4. Core data

Every timing knob is parsed from a `<integer><unit>` string (`ms|s|m|h`, see `duration.rs`)
into a `Duration` once at load time; there are no derived formulas or multipliers (e.g. no
"10x interval" floor, no "battery multiplier" — `battery_interval` is its own explicit knob).

```rust
struct Config { daemon: DaemonConfig, badge: BTreeMap<String, BadgeConfig> }

// Every field already resolved to a concrete value (badge-level unset -> this; see below).
struct DaemonConfig {
    interval: Duration,          // default 60s: fallback refresh period
    battery_interval: Duration,  // default: same as `interval` (no multiplier)
    active_window: Duration,     // default 5m: a key is active this long after last_access
    coalesce: Duration,          // default 1s: batching window for the timer; must be < every interval
    retry_min: Duration,         // default 2s: backoff floor
    retry_max: Duration,         // default 5m: backoff cap
    timeout: Duration,           // default 2s: fallback hard kill for the whole fetch + extract
    power_check: Duration,       // default 60s: how often on_ac() is re-checked
    config_check: Duration,      // default 2s: how often the config file's mtime is polled
    cold_wait: Duration,         // default 20ms: must stay < the client's 40ms deadline
    idle_exit: Duration,         // default 30m: exit after no client requests for this long
    path_evict: Duration,        // default 30m: idle path-scoped keys are dropped after this
    max_paths: usize,            // default 256: LRU cap on path-scoped (git) cache entries
    max_output: usize,           // default 64 KiB: fallback cap on command stdout / HTTP body
}

struct BadgeConfig {
    source: Source,
    extract: Option<Extract>,
    format: String,              // default "{value}"
    interval: Duration,          // badge value -> daemon.interval
    battery_interval: Duration,  // badge value -> badge.interval (if set) -> daemon.battery_interval -> daemon.interval
    active_window: Duration,     // badge value -> daemon.active_window
    timeout: Duration,           // badge value -> daemon.timeout
    max_output: usize,           // badge value -> daemon.max_output
    scope: Scope,                // badge `scope = "..."` -> default_scope(source), see below
}

// Default is GitRoot for a path-scoped builtin (`Builtin::is_path_scoped`), Global otherwise;
// git builtins require GitRoot; other sources can override with "git_root" | "global".
enum Scope { Global, GitRoot }

enum Builtin {
    GitBranch, GitStatus,
    GitCounts,           // one of GitField's counts; requires `field = "..."`
    GitCommit, GitState, GitStash,  // in-process, no subprocess (see §5)
    Battery, Hostname, LoadAvg, MemUsedPercent, Uptime,
}
// is_path_scoped(): the six Git* variants; everything else is global.

enum Source {
    Builtin { name: Builtin, device: Option<String> },  // `device`: battery only, e.g. "BAT0"
    Command { command: String, args: Option<Vec<String>> },  // Some -> exec argv; None -> `sh -c command`
    Http { url: String, headers: BTreeMap<String, String> },
}

enum Extract {
    Trim,                                      // implicit default
    Regex { re: Regex, group: usize },         // compiled once at config load
    Json { pointer: String },                  // serde_json Value::pointer
}
```

Deserialization goes through a separate `RawBadgeConfig`/`RawDaemonConfig` first: every
timing field is an `Option<String>` (the raw duration text, or unset), with
`#[serde(deny_unknown_fields)]` so config typos are caught. `RawBadgeConfig` does *not* use
`#[serde(flatten)]` for its per-variant fields — flatten and `deny_unknown_fields` don't
combine in serde, so each `Builtin`/`Command`/`Http` variant repeats the shared fields
(`format`, `interval`, `battery_interval`, `active_window`, `timeout`, `max_output`,
`extract`) instead. `Config::load` then resolves every `Option<String>` against its
`[daemon]` value or fixed default and parses it with `duration::parse`, and validates the
result (`validate`, e.g. `retry_min <= retry_max`, `coalesce` below every resolved interval,
`cold_wait` below the 40 ms client deadline) before returning — a config that fails to parse
or validate is rejected wholesale; the caller (`daemon::check_reload`) keeps the previous
config running.

```rust
type Key = (String /* badge */, Option<PathBuf> /* git root; None = global */);

struct Entry {
    value: Option<String>,     // last GOOD rendered value; never cleared by a failure
    fetched_at: Option<Instant>,
    last_access: Instant,      // last client request touching this key
    errors: u32,
    next_attempt: Instant,     // backoff gate after failures
    in_flight: bool,           // single-flight
    // `.git/HEAD` + `.git/index` mtimes as of the fetch this entry last *started*; only set
    // for `git_status`/`git_counts` (see §5/§6's fingerprint invalidation). `None` for every
    // other badge, including the four in-process git builtins, which never get an Entry at
    // all (handle_get bypasses the cache for them entirely).
    fingerprint: Option<GitFingerprint>,
}
```

Cache is a `HashMap<Key, Entry>` inside a `std::sync::Mutex` (single-threaded runtime, never held
across `.await`). Scope is structural: git builtins are keyed by repo root, everything else is global.
Hard caps: max `max_paths` path-scoped keys (default 256, LRU by `last_access`), enforced
immediately whenever a new path-scoped key is inserted (`handle_get`), not only from the timer
path — otherwise a config with only path-scoped badges could overshoot it for up to `idle_exit`.
Idle path-scoped keys (see §6) are dropped from the same request path too, throttled to once
every `config_check` (default 2s), same as `check_reload`.

Backoff: `min(retry_min * 2^errors, retry_max)` ±20% jitter (defaults 2s/5min).

## 5. Provider execution

**Subprocess** (command badges, git builtins):

```rust
async fn run_with_timeout(prog, args, cwd, deadline, cap) -> Result<Vec<u8>> {
    let mut child = tokio::process::Command::new(prog)
        .args(args).current_dir(cwd)
        .stdin(null())
        .stdout(piped()).stderr(null())
        .process_group(0)          // own group, killable as a unit
        .kill_on_drop(true)        // safety net if the future is dropped
        .spawn()?;
    let pgid = child.id();
    let work = async {
        // capped stdout read AND wait() both happen INSIDE the timeout.
        let out = read_capped(stdout, cap).await?;
        let status = child.wait().await?;
        ...
    };
    match tokio::time::timeout_at(deadline, work).await {
        Ok(r) => r,
        // Leader is not yet reaped here (zombie at worst), so the pgid cannot have been recycled.
        Err(_) => { killpg(pgid, SIGKILL); child.wait().await; Err(Timeout) }
    }
}
```

No `killpg` on the success path: once the leader is reaped its pgid may be reused. A grandchild
that keeps stdout open prevents EOF, so it ends up on the timeout path and gets killed with the group.
A grandchild that closes stdout and daemonizes itself is the user's explicit choice and is left alone.

**In-flight registry**: the daemon keeps a `HashSet<pgid>` of running provider groups (inserted on
spawn, removed after reap or when the owning future is dropped, which kills the group). `stop`
closes admission before the sweep: a spawn that returns afterwards kills its own group instead of
registering it. On `stop`/SIGTERM/SIGINT/idle-exit it `killpg`s every entry before exiting;
`kill_on_drop` alone only kills the leader.

Invariants: stdin is never the tty; `Command::spawn` (which waits for exec/chdir) runs on the
four-slot blocking pool; nothing that can block is awaited outside the
deadline; the process group is always killed and the child always reaped. Stdout past `cap` →
`TooLarge` error (and kill). Env var `GIT_OPTIONAL_LOCKS=0` is set for git to avoid index lock
contention with the user's own git commands.

**HTTP**: one `reqwest::Client` (connect timeout 2s, pool idle timeout 30s so idle sockets don't keep
the radio/timers busy). Each request is wrapped in `timeout_at(deadline)`; body read by `chunk()` with
the byte cap. Non-2xx → error. DNS goes through `getaddrinfo` on tokio's blocking pool, which a
timeout cannot cancel; the runtime is built with `max_blocking_threads(4)`. Filesystem,
process-spawn, and extraction jobs also use that runtime pool, with a separate four-slot
admission limit held until actual completion. A stuck resolver or mount cannot block the event
loop, although it can exhaust blocking-worker capacity. Shutdown uses a zero blocking-worker
wait.

**Extraction** runs on a blocking worker within the same per-badge deadline, in-process
(no subprocess); semaphore admission time counts against that deadline too.

**Concurrency**: global `Semaphore(4)` acquired per fetch. Single-flight via `Entry.in_flight`.

**Git root discovery** is in-process (walk up from cwd, `stat` for `.git` file or dir), never a
subprocess on the request path. It runs on a bounded worker only for requests that need a
repository. Non-repo cwd or expired/saturated filesystem work → git badges render empty with
no fetch. Regular git directories, directory symlinks, and worktree `gitdir:` files are supported.

**In-process git builtins** (`git_branch`/`git_commit`/`git_state`/`git_stash`,
`provider::run_in_process_git`): each is a handful of file reads (parse `HEAD`'s `ref:
refs/heads/<name>` line or lack thereof, check for `MERGE_HEAD`/`rebase-merge`/etc., count
the stash reflog's lines via `commondir`-resolved shared git dir). Normally cheap enough to
recompute from scratch on *every* `get`, but file reads can hang on remote mounts, so
`daemon::handle_get` takes one worker snapshot of the requested git values and fingerprint
before touching the cache or `provider::fetch`. These four badges never get a `cache::Entry`
and have no stale-while-revalidate window or fs watcher. On timeout/saturation they render
empty. `run_builtin`'s match arm for the same
four builtins is kept only as a structurally-unreachable fallback (nothing on the real request
path calls it once `handle_get` intercepts first).

**Git fingerprinting** (`provider::git_fingerprint`, `GitFingerprint { head, index }`): mtimes
of `.git/HEAD` and `.git/index` (gitdir-resolved), used as a cheap, subprocess-free proxy for
"has the repo's branch or stage changed". `git_status`/`git_counts` are the two git builtins
still backed by a `git` subprocess (status is relatively expensive to compute in-process), so
instead of recomputing per request they keep their normal cache entry, but `handle_get`
compares the fingerprint at `get`-time against the one stored on the `Entry` when its last
fetch started; a mismatch forces the entry stale *and* cold, so the refreshing fetch lands in
the same prompt via `cold_wait` instead of waiting out `interval`. This catches `git add`,
`git commit`, `git checkout -b`, etc. — anything that touches `HEAD` or the index. Unstaged
worktree edits touch neither file, so they're still only picked up on `interval`; closing that
gap would need a watcher on the whole worktree, which this crate otherwise avoids.

**`git_status`/`git_counts` dedupe** (`provider::porcelain_fetch`): concurrent `git_status` and
`git_counts` badges for the same repo share `git status --porcelain=v2 --branch` work. Completed
results are not kept as a second permanent repository cache: badge entries own freshness and TTL
refreshes fetch new worktree state even when HEAD/index have not changed. Each caller enforces
its own deadline and output cap while sharing work, but the shared subprocess runs with the
creator's limits: a joiner with looser limits can fail conservatively.

## 6. Scheduling and power efficiency

Goal: near-zero wakeups when the user isn't looking at a prompt.

- **Demand-driven.** A key is *active* while `now - last_access < active_window` (the badge's
  resolved `active_window`, default 5 min — a fixed configured value, not a formula derived
  from `interval`). Inactive keys are never refreshed; the next `get` re-activates them and
  returns the stale value (stale-while-revalidate). Path-scoped keys idle for `path_evict`
  (default 30 min) are evicted.
- **Refresh on request.** `get` for a key whose value is older than its resolved
  `interval`/`battery_interval` (`cache::effective_interval`, AC-aware — same function the
  timer-driven path below uses, so `handle_get` and the timer never disagree about what
  "stale" means) and not backing off/in flight spawns a background refresh task and returns
  immediately. For path-scoped keys this is the only trigger — git state only needs
  refreshing when someone renders a prompt.
- **In-process git builtins skip this entirely.** `git_branch`/`git_commit`/`git_state`/
  `git_stash` are recomputed on a bounded blocking worker per request (see §5) — no `Entry`, no
  staleness check, no background task; every `get` sees the current on-disk state.
- **Fingerprint invalidation for `git_status`/`git_counts`.** On top of the interval check
  above, `handle_get` also compares a freshly-read `GitFingerprint` (`.git/HEAD` +
  `.git/index` mtimes) against the one recorded when the entry's current value was fetched; a
  mismatch forces the entry stale *and* treats it as a cold miss, so the refresh is awaited
  (bounded by `cold_wait`) instead of answering with the old value and refreshing in the
  background. This is what makes `git checkout -b`/`git add`/`git commit` visible in the very
  next prompt instead of after up to `interval` — see §5 for what it does and doesn't catch.
- **One timer.** The daemon loop is `select!{ accept, sleep_until(target), sigterm, sigint,
  shutdown, reschedule }`, where `target` is `min(next_due, idle_deadline)`. `next_due` is the
  min over active global keys of `max(fetched_at + interval, next_attempt)` (`interval` or
  `battery_interval` depending on AC state, see below). No per-badge tasks, no fixed tick. A
  finished background fetch notifies `reschedule` so the loop recomputes `next_due` right away
  instead of waiting for the next wakeup. When nothing is active the timer is `idle_exit` only.
- **Coalescing.** When the timer fires, every active global key due within `coalesce` (default
  1s) of that moment is refreshed in the same batch, so badges converge onto shared wakeups.
  `coalesce` must stay below every resolved interval (enforced at config load), or refreshes due
  seconds apart could collapse into the same wakeup as ones due a full interval apart.
- **On battery**: on Linux, on AC if any `/sys/class/power_supply/*` whose `type` is not `Battery`
  (Mains, USB, Wireless, …) has `online == 1`; if no such supply exists, assume AC (desktop).
  On macOS, the `Now drawing from '…'` header of `pmset -g batt`: `'AC Power'` is AC, any other
  source is battery; a failed, unparseable, or >400 ms probe (killed) counts as AC.
  Re-checked at most once per `power_check` (default 60s). Off AC, badges use their resolved
  `battery_interval` instead of `interval` — a separate configured value, not a multiplier; it
  defaults to the same value as `interval` when unset (see §4/§7 for the full fallback chain).
- **Idle exit.** No client request for `idle_exit` (default 30 min) → daemon exits. The next
  `get` respawns it.
- **No fs watchers.** Config is reloaded by `stfgd reload`, and additionally the daemon `stat`s
  the config file at most once per `config_check` (default 2s) on incoming requests and reloads
  if mtime changed. A config that fails to parse or validate is logged and the previous config
  kept running (on initial daemon startup, where there is no previous config yet, an empty
  default config is used instead and logged the same way). Implicit checks run in the
  background, with no filesystem access on the event loop; explicit reload waits at most
  500 ms. Successful reload invalidates active entries, wakes the scheduler, and prevents
  old-config fetch completions from overwriting new-config state.
- **Cold-miss wait.** A `get` on a key with *no value at all* (first `cd` into a repo) waits up
  to the configured `cold_wait` (default 20 ms) for the fetch it just started, then answers with
  whatever it has. The fetch keeps running in the background either way. Config validation
  rejects a `cold_wait` at or above the client's 40 ms deadline. Request-time git reads and
  cold fetch waits share a single budget of `max(20ms, cold_wait)`, rather than adding
  independent waits for each badge.

## 7. Config example

Every timing value is a `<integer><unit>` string (`ms|s|m|h`, see `duration.rs`/§3).

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

[badge.git_stash]
type = "builtin"
name = "git_stash"

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

starship:

```toml
[custom.sf_public_ip]
command = "public_ip"                 # badge name(s), appended as argv
shell = ["stfg"]                      # tiny std+libc client, no `sh -c` layer
use_stdin = false
when = true                           # custom modules are hidden by default without when/detect_*
format = "[$output ]($style)"
style = "bg:color_bg_l1 fg:color_fg_l1"
```

Running `stfg` as the "shell" skips both the per-module `sh -c` fork and the full
binary's startup cost (see §9 for measured numbers). Starship runs custom commands in the
prompt's cwd, so git badges resolve correctly.

Empty output ⇒ starship hides the module.

## 8. CLI

```sh
stfgd get <badge>... [--cwd <path>]   # hot path, never fails, no stderr
stfgd daemon                          # foreground
stfgd status                          # table of badges/keys, age, errors; 1s timeout
stfgd stop
stfgd reload
```

`stfg <badge>... [--cwd <path>]` is a separate, tiny binary that is an exact equivalent
to `stfgd get`: same `client.rs` implementation, same flags, same behavior, just without the
full binary's dependency weight. Prefer it for the hot path (starship/tmux); use `stfgd get`
only where installing a second binary isn't convenient. There is no `stfg daemon` /
`status` / etc. — admin commands stay on the full binary.

## 9. Dependencies

`tokio` (`rt, macros, net, process, time, sync, io-util, signal`), `serde` (derive), `toml`,
`serde_json`, `regex`, `reqwest` (`default-features = false`, `rustls`), `libc`
(`flock`, `killpg`, `setsid`, and the client's non-blocking `socket`/`connect`/`fcntl`). No
clap/anyhow/thiserror/git2/gix/notify.

## 10. Failure modes

| Scenario | Behavior |
|---|---|
| Daemon not running | client spawns detached, returns empty now |
| Stale socket file | connect refused → spawn; the new daemon unlinks it after taking the lock |
| Runtime dir is a symlink, foreign-owned, or group/other-writable | client prints empty lines and never spawns; a daemon started anyway exits without binding unless the dir is a real one owned by `getuid()` |
| Daemon wedged/slow | client deadline (40 ms) → empty, exit 0 |
| Two clients race to spawn | loser daemon fails flock, exits 0 |
| Command hangs | killpg at `timeout` (default 2s), reaped, error counted, backoff, stale value kept |
| Command reads stdin | gets EOF from /dev/null |
| Command backgrounds a grandchild | killed with the group after main process exits |
| Command floods stdout | cut at `max_output` (default 64 KiB), killed, error |
| HTTP slow/unreachable | deadline → error, backoff, stale value kept |
| Config broken on reload | logged, previous config kept |
| Config broken at daemon startup | logged (stderr only), daemon runs with zero badges until fixed |
| Not a git repo | git badges empty, no subprocess |
| Many repos visited | path-scoped keys evicted after `path_evict` idle (default 30 min), LRU cap `max_paths` (default 256) |
| Laptop idle | no requests → keys go dormant → zero wakeups → daemon exits after `idle_exit` (default 30 min) |
| On battery | badges use their resolved `battery_interval` instead of `interval` |
| Binary upgraded | version mismatch on a `get` → daemon answers, then shuts itself down; next prompt spawns new |
| `stop`/SIGTERM with in-flight fetches | every registered pgid is `killpg`ed, then exit; lock released by kernel, socket unlinked only if its dev/ino are still the ones this daemon bound |
| `accept()` fails (e.g. `EMFILE`) | wait 20 ms, then retry — no busy spin |

## 11. Testing

Unit: config parse (all variants, invalid → error; `scope` default/override/rejection),
extract (regex/json/trim/template empty), backoff monotonic/capped, bounded line reader,
scheduling `next_due`/coalescing, git root discovery, `HEAD` parsing (branch/detached/
worktree `gitdir:`), git fingerprint change detection, `git_counts` dedupe (an in-flight
memo cell is reused, not replaced), and `handle_get` picking the battery-aware interval.

Integration (`tests/integration.rs`): run the real `stfgd`/`stfg` binaries with a temp
`XDG_RUNTIME_DIR` + `STAR_FORGE_CONFIG`:

1. `get` on a badge running `sleep <marker>` (no `args` -> `sh -c`) with `timeout = "200ms"`
   returns within the deadline, exit 0; after ~300 ms no process with the marker exists
   (killed + reaped).
2. `get` with no daemon returns empty quickly and a later `get` returns the value.
3. A pipeline command (`sleep <marker> | sleep <marker>`, no `args`) is killed as a whole group
   on timeout — both stages gone, proving `killpg` covers every stage, not just the shell leader.
4. A global badge with no client traffic still refreshes on its own timer (the timer is re-armed
   when a background fetch finishes, not only on the next request).
5. `stfgd reload` re-resolves the config: a 1s-interval badge stops changing once the config is
   rewritten with a 1h interval and reloaded.
6. `stfgd get` (the full binary's subcommand) converges to a real value exactly like `stfg`,
   proving both entry points share the one `client.rs` implementation.
7. A real temp repo: after `git checkout -b <new>`, `get git_branch` reports it, and after
   `git add` on a tracked file `get git_status` shows the new count, both well inside the 5s
   `interval` (fingerprint invalidation, not waiting it out).

Regression coverage also exercises worktree-only `git_counts` TTL refreshes, shortening
an interval without further requests, rejected git scope on startup/reload, and FIFO-backed
blocked git/config reads while cached global values and admin requests remain responsive.
Unit tests check worker saturation/late-result disposal and prevent old-config or evicted
fetches from overwriting replacement entries. Client tests cover one absolute budget
across preparation, partial writes, trickled reads, and stalled operations.

Everything runs on Linux and macOS (CI tests on `aarch64-apple-darwin` and lints both Darwin
targets): processes are found with `ps -A -ww -o args=` (not `/proc`), daemon liveness is a
non-blocking `flock(LOCK_SH)` probe of `daemon.lock` (not `lsof`), test configs avoid GNU-only
tools (`date +%N`, `timeout`, `stat -c`, …), and temp dirs live directly under `/tmp` so socket
paths stay well inside macOS's 104-byte `sun_path`.
