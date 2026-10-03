//! End-to-end tests against the real binary (per design.md §11): a temp
//! `XDG_RUNTIME_DIR` + `STAR_FORGE_CONFIG` per test, run through `Command::env` (not
//! `std::env::set_var`) so parallel `cargo test` runs never race on process-global env.

use std::ffi::CString;
use std::fmt::Write as _;
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_stfgd");
const GET_BIN: &str = env!("CARGO_BIN_EXE_stfg");

/// Isolated runtime dir + config for one test; cleans up (stops the detached daemon,
/// removes the dir) on drop, including on panic/unwind.
struct TestEnv {
    dir: PathBuf,
}

impl TestEnv {
    fn new(config_toml: &str) -> Self {
        // macOS clocks tick in microseconds, so parallel tests can read the same timestamp;
        // without the counter two tests would share (and delete) one runtime dir and config.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        // Not `temp_dir()`: macOS's `/var/folders/...` would push the socket path past
        // `sun_path`'s 104 bytes.
        let dir = PathBuf::from(format!("/tmp/sf-it-{}-{seq}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::File::create(&config_path)
            .unwrap()
            .write_all(config_toml.as_bytes())
            .unwrap();
        Self { dir }
    }

    fn config_path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    fn socket_path(&self) -> PathBuf {
        self.dir.join("star-forge/sock")
    }

    /// `stfgd`/`stfg` with this env's runtime dir/config. The client deadline is raised from
    /// the 40 ms default: these tests check behavior, and a loaded CI runner (notably macOS)
    /// regularly spends more than 40 ms just starting the client or spawning the daemon.
    /// Daemon-side budgets (`cold_wait`, request-time git reads) are unchanged.
    fn command(&self, bin: &str) -> Command {
        let mut command = Command::new(bin);
        command
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("STAR_FORGE_CONFIG", self.config_path())
            .env("STAR_FORGE_TIMEOUT_MS", "1000");
        command
    }

    /// Runs `stfgd <args>` with this env's runtime dir/config, returns (stdout, ok).
    fn run(&self, args: &[&str]) -> (String, bool) {
        let out = self.command(BIN).args(args).output().expect("spawn stfgd");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            out.status.success(),
        )
    }

    /// Runs the tiny `stfg` binary directly (the real hot path; per
    /// `design.md` §2/§3, `stfgd get` is the exact same shared `client`
    /// implementation, just with one extra `mod` indirection, so exercising this one
    /// covers both).
    fn get(&self, badges: &[&str]) -> (Vec<String>, bool) {
        let out = self
            .command(GET_BIN)
            .args(badges)
            .output()
            .expect("spawn stfg");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let lines = stdout.lines().map(str::to_string).collect();
        (lines, out.status.success())
    }

    /// Same as `get`, but with an explicit `--cwd` (path-scoped/git badges key off this
    /// instead of the test process's own cwd).
    fn get_in(&self, badges: &[&str], cwd: &Path) -> (Vec<String>, bool) {
        let out = self
            .command(GET_BIN)
            .args(badges)
            .arg("--cwd")
            .arg(cwd)
            .output()
            .expect("spawn stfg");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let lines = stdout.lines().map(str::to_string).collect();
        (lines, out.status.success())
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.command(BIN)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn stfgd")
    }

    fn spawn_get_in(&self, badges: &[&str], cwd: &Path) -> Child {
        self.command(GET_BIN)
            .args(badges)
            .arg("--cwd")
            .arg(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn stfg")
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = self.run(&["stop"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Bounded-retry poll: calls `f` every `interval` until it returns `true` or `timeout`
/// elapses. Never sleeps/blocks past `timeout`, so it can't turn a real failure into a hang.
fn wait_until(timeout: Duration, interval: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(interval);
    }
}

/// Count of live processes whose argv is exactly `sleep <marker>`. `ps -A -ww -o args=` means
/// the same on procps (Linux) and BSD `ps` (macOS); zombies print as `[sleep] <defunct>` or
/// `(sleep)`, so they never count. A missing `ps` panics instead of vacuously reporting zero.
fn proc_cmdline_count(marker: &str) -> usize {
    let out = Command::new("ps")
        .args(["-A", "-ww", "-o", "args="])
        .output()
        .expect("run ps");
    let want = format!("sleep {marker}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.trim() == want)
        .count()
}

fn wait_for_socket(env: &TestEnv) {
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            env.socket_path().exists()
        }),
        "daemon never created its socket at {}",
        env.socket_path().display()
    );
}

fn wait_for_child(mut child: Child, timeout: Duration) -> Output {
    let finished = wait_until(timeout, Duration::from_millis(5), || {
        child.try_wait().expect("wait for child").is_some()
    });
    if !finished {
        let _ = child.kill();
        let _ = child.wait();
        panic!("child did not exit within {timeout:?}");
    }
    child.wait_with_output().expect("collect child output")
}

/// Whether a daemon holds this env's `flock` singleton lock: a non-blocking shared lock only
/// fails while another process holds the exclusive one. Portable and needs no `/proc` or
/// `lsof` (absent on minimal Linux, slow on macOS). Only probe once the daemon already owns
/// the lock (socket up, or stopping): a daemon starting during the brief shared hold would
/// lose its `LOCK_EX | LOCK_NB` and exit.
fn daemon_running(env: &TestEnv) -> bool {
    let Ok(lock) = std::fs::File::open(env.dir.join("star-forge/daemon.lock")) else {
        return false;
    };
    // SAFETY: flock on an fd owned by `lock`, which outlives the call; dropping `lock`
    // releases the shared lock if it was granted.
    let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if rc == 0 {
        return false;
    }
    let error = std::io::Error::last_os_error();
    assert_eq!(
        error.raw_os_error(),
        Some(libc::EWOULDBLOCK),
        "flock probe failed: {error}"
    );
    true
}

fn wait_for_daemon(env: &TestEnv) {
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            daemon_running(env)
        }),
        "daemon process for {} never appeared",
        env.dir.display()
    );
}

fn wait_for_daemon_exit(env: &TestEnv) {
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            !daemon_running(env)
        }),
        "daemon process for {} did not exit",
        env.dir.display()
    );
}

fn mkfifo(path: &Path) {
    let path_c = CString::new(path.as_os_str().as_bytes()).expect("FIFO path has no NUL");
    // SAFETY: `path_c` is NUL-terminated and points to a valid path; the mode is a valid
    // permission mask for `mkfifo(3)`.
    let result = unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) };
    assert_eq!(
        result,
        0,
        "mkfifo {} failed: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

fn open_fifo_writer(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

/// A FIFO writer opened only after the daemon's reader is waiting. Keeping it open stalls
/// `read_to_string`; dropping it releases the reader with EOF, including during unwinding.
struct FifoStall {
    path: PathBuf,
    writer: Option<std::fs::File>,
    released: bool,
}

impl FifoStall {
    fn create(path: &Path) -> Self {
        mkfifo(path);
        Self {
            path: path.to_path_buf(),
            writer: None,
            released: false,
        }
    }

    /// Opens the writer if a reader is already blocked on the FIFO; `false` while none is.
    fn try_attach_writer(&mut self) -> bool {
        match open_fifo_writer(&self.path) {
            Ok(file) => {
                self.writer = Some(file);
                true
            }
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => false,
            Err(error) => panic!("open FIFO writer {} failed: {error}", self.path.display()),
        }
    }

    fn wait_for_reader(&mut self) {
        assert!(
            wait_until(Duration::from_secs(2), Duration::from_millis(5), || {
                self.try_attach_writer()
            }),
            "no reader opened FIFO {}",
            self.path.display()
        );
    }

    fn release(&mut self) {
        self.writer.take();
        self.released = true;
    }
}

impl Drop for FifoStall {
    fn drop(&mut self) {
        if self.writer.is_some() || self.released {
            return;
        }
        // On a failing assertion, a worker may be between dispatch and its blocking FIFO
        // open. Give it a bounded chance to rendezvous before the temp directory is removed.
        let _ = wait_until(Duration::from_millis(250), Duration::from_millis(5), || {
            match open_fifo_writer(&self.path) {
                Ok(file) => {
                    self.writer = Some(file);
                    true
                }
                Err(error) => error.raw_os_error() != Some(libc::ENXIO),
            }
        });
    }
}

/// design.md §11.1: a hanging command is killed+reaped at `timeout`, never blocking the
/// client past its own deadline.
#[test]
fn hang_test_process_is_killed_and_reaped() {
    // A fractional-second count unique enough not to collide with any real process; `sleep`
    // treats it as a duration, so the process just becomes a long-lived, greppable marker.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let marker = format!("{}.{}", 9_000_000 + std::process::id(), nanos % 1000);

    let env = TestEnv::new(&format!(
        r#"
[daemon]
coalesce = "100ms"

[badge.hang]
type = "command"
command = "sleep"
args = ["{marker}"]
interval = "1s"
timeout = "1s"
"#
    ));

    // First `get`: no daemon yet, connect fails, daemon is spawned detached, empty now.
    let (lines, ok) = env.get(&["hang"]);
    assert!(ok, "get must always exit 0");
    assert_eq!(lines, vec![String::new()]);

    wait_for_socket(&env);

    // Second `get`: daemon is up, this is the cold miss that starts the hanging fetch.
    let start = Instant::now();
    let (lines, ok) = env.get(&["hang"]);
    assert!(ok);
    assert_eq!(lines.len(), 1, "one line per requested badge");
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "get must never block on a slow provider, took {:?}",
        start.elapsed()
    );

    // The marker process should show up soon (fetch spawned in the background).
    assert!(
        wait_until(Duration::from_secs(1), Duration::from_millis(20), || {
            proc_cmdline_count(&marker) > 0
        }),
        "hanging process with marker {marker} never started"
    );

    // Soon after timeout=1s it must be killed and reaped (retry_min=2s keeps it from
    // respawning before this check): bounded-retry, not a flaky fixed sleep.
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            proc_cmdline_count(&marker) == 0
        }),
        "process with marker {marker} was not killed+reaped after timeout"
    );
}

/// design.md §11.3: no `args` -> `sh -c <command>`, so a pipeline is killed as a whole
/// process group on timeout — both stages, not just the shell leader.
#[test]
fn hang_pipeline_is_killed_in_every_stage() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let marker = format!("{}.{}", 9_100_000 + std::process::id(), nanos % 1000);

    let env = TestEnv::new(&format!(
        r#"
[daemon]
coalesce = "100ms"

[badge.hang_pipe]
type = "command"
command = "sleep {marker} | sleep {marker}"
interval = "1s"
timeout = "1s"
"#
    ));

    let (lines, ok) = env.get(&["hang_pipe"]);
    assert!(ok, "get must always exit 0");
    assert_eq!(lines, vec![String::new()]);

    wait_for_socket(&env);

    let (lines, ok) = env.get(&["hang_pipe"]);
    assert!(ok);
    assert_eq!(lines.len(), 1, "one line per requested badge");

    // Both pipeline stages should show up soon (fetch spawned in the background).
    assert!(
        wait_until(Duration::from_secs(1), Duration::from_millis(20), || {
            proc_cmdline_count(&marker) >= 2
        }),
        "both pipeline stages with marker {marker} never started"
    );

    // Soon after timeout=1s every stage must be killed and reaped (retry_min=2s keeps it
    // from respawning before this check): bounded-retry, not a flaky fixed sleep.
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            proc_cmdline_count(&marker) == 0
        }),
        "a pipeline stage with marker {marker} survived past timeout"
    );
}

/// design.md §5: `Shared::stop` closes the global `Semaphore(4)` before killing every
/// registered process group. Six queued two-stage pipeline badges prove both halves at once:
/// at most 4 groups (8 marker processes, two stages each) ever run concurrently, `stop` kills
/// every one of them, and the fetches still queued behind the semaphore never start a new
/// group afterward.
#[test]
fn stop_kills_every_running_group_and_queued_fetches_never_start() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let marker = format!("{}.{}", 9_200_000 + std::process::id(), nanos % 1000);

    let mut config = String::from(
        r#"
[daemon]
coalesce = "100ms"
retry_min = "100ms"
"#,
    );
    for name in ["p1", "p2", "p3", "p4", "p5", "p6"] {
        write!(
            config,
            r#"
[badge.{name}]
type = "command"
command = "sleep {marker} | sleep {marker}"
interval = "60s"
timeout = "30s"
"#
        )
        .unwrap();
    }
    let env = TestEnv::new(&config);
    let badges = ["p1", "p2", "p3", "p4", "p5", "p6"];

    let (_, ok) = env.get(&badges);
    assert!(ok, "get must always exit 0");

    wait_for_socket(&env);

    // Daemon is up: this schedules all six fetches at once, four taking a semaphore permit
    // and two staying queued behind it. Under load a spawn can fail softly when the blocking
    // pool is saturated; the short `retry_min` lets that badge retry well inside the wait.
    let (_, ok) = env.get(&badges);
    assert!(ok);

    assert!(
        wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
            proc_cmdline_count(&marker) >= 8
        }),
        "four two-stage pipeline groups (8 marker processes) never started"
    );
    let running = proc_cmdline_count(&marker);
    assert!(
        running <= 8,
        "semaphore cap of 4 was not enforced: {running} marker processes running"
    );

    let (_, ok) = env.run(&["stop"]);
    assert!(ok, "stop request failed");

    assert!(
        wait_until(Duration::from_secs(3), Duration::from_millis(20), || {
            proc_cmdline_count(&marker) == 0
        }),
        "a running pipeline group survived stop"
    );

    // The two fetches still queued behind the semaphore must never start a new group after
    // stop closed admission: poll for a while and confirm the count never leaves zero.
    let restarted = wait_until(
        Duration::from_millis(500),
        Duration::from_millis(20),
        || proc_cmdline_count(&marker) != 0,
    );
    assert!(!restarted, "a queued fetch started a new group after stop");
}

/// design.md §11.2: no daemon yet -> empty fast; once the daemon is up, a later `get`
/// returns the real value. Runs the tiny `stfg` binary directly (via
/// `TestEnv::get`): the cold-start case spawns the daemon from the `stfgd` binary
/// sitting next to it in the same `target/<profile>` dir, exercising the real
/// sibling-binary spawn path (`client::spawn_daemon_detached`), not just `current_exe()`.
#[test]
fn no_daemon_then_value() {
    let env = TestEnv::new(
        r#"
[badge.echo]
type = "command"
command = "echo"
args = ["hello"]
interval = "60s"
timeout = "2s"
"#,
    );

    let start = Instant::now();
    let (lines, ok) = env.get(&["echo"]);
    assert!(ok);
    assert_eq!(lines, vec![String::new()]);
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "no-daemon get must return fast, took {:?}",
        start.elapsed()
    );

    wait_for_socket(&env);

    let got = wait_until_value(&env, "echo", "hello", Duration::from_secs(2));
    assert!(got, "badge never converged to the real value");
}

/// Repeatedly calls `get <badge>` until its value equals `want` or `timeout` elapses.
fn wait_until_value(env: &TestEnv, badge: &str, want: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let (lines, ok) = env.get(&[badge]);
        if ok && lines.first().map(String::as_str) == Some(want) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Global badges refresh on their interval with no client traffic: the timer must be re-armed
/// when a background fetch finishes, not only on the next request.
#[test]
fn global_badge_refreshes_on_timer_without_requests() {
    // `echo $$` prints the fresh `sh -c` pid, so every fetch yields a new value. Not
    // `date +%s%N`: BSD `date` (macOS) has no `%N`.
    let env = TestEnv::new(
        r#"
[daemon]
coalesce = "100ms"

[badge.now]
type = "command"
command = "echo $$"
interval = "1s"
"#,
    );
    let _ = env.get(&["now"]);
    wait_for_socket(&env);
    let mut first = String::new();
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            first = env.get(&["now"]).0.concat();
            !first.is_empty()
        }),
        "badge never produced a value"
    );

    // No requests for a few intervals. A `get` returns the cached value without waiting,
    // so a changed value proves the daemon refreshed on its own.
    std::thread::sleep(Duration::from_millis(3500));
    let (lines, ok) = env.get(&["now"]);
    assert!(ok);
    assert_ne!(
        lines.concat(),
        first,
        "value was not refreshed by the timer"
    );
}

/// `stfgd reload` re-resolves the config: a badge refreshing every second must stop
/// changing once the config is rewritten with a 1h interval and reloaded.
#[test]
fn reload_picks_up_a_new_interval() {
    let env = TestEnv::new(
        r#"
[daemon]
coalesce = "100ms"

[badge.now]
type = "command"
command = "echo $$"
interval = "1s"
"#,
    );
    let _ = env.get(&["now"]);
    wait_for_socket(&env);
    let mut first = String::new();
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            first = env.get(&["now"]).0.concat();
            !first.is_empty()
        }),
        "badge never produced a value"
    );
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            env.get(&["now"]).0.concat() != first
        }),
        "badge never refreshed on the 1s timer before reload"
    );

    // Rewrite the config with a 1h interval and reload.
    std::fs::File::create(env.config_path())
        .unwrap()
        .write_all(
            br#"
[badge.now]
type = "command"
command = "echo $$"
interval = "1h"
"#,
        )
        .unwrap();
    let (_, ok) = env.run(&["reload"]);
    assert!(ok, "reload must succeed");

    // Value settles, then must stay put across further intervals-worth of wall time.
    std::thread::sleep(Duration::from_millis(200));
    let settled = env.get(&["now"]).0.concat();
    std::thread::sleep(Duration::from_millis(3000));
    let (lines, ok) = env.get(&["now"]);
    assert!(ok);
    assert_eq!(
        lines.concat(),
        settled,
        "badge kept refreshing after reload raised the interval to 1h"
    );
}

/// `stfgd get` (the full binary's subcommand) and `stfg` (the tiny
/// binary) share exactly one `client` implementation (design.md §3): this checks the
/// full binary's entry point also gets a real value, not just the tiny one exercised by
/// every other test in this file via `TestEnv::get`.
#[test]
fn full_binary_get_subcommand_shares_the_same_client() {
    let env = TestEnv::new(
        r#"
[badge.echo]
type = "command"
command = "echo"
args = ["parity"]
interval = "60s"
timeout = "2s"
"#,
    );

    let (lines, ok) = env.run(&["get", "echo"]);
    assert!(ok, "stfgd get must always exit 0");
    assert_eq!(lines.lines().collect::<Vec<_>>(), vec![""]);

    wait_for_socket(&env);

    let deadline = Instant::now() + Duration::from_secs(2);
    let got = loop {
        let (out, ok) = env.run(&["get", "echo"]);
        assert!(ok);
        let got = out.lines().next().unwrap_or_default().to_string();
        if got == "parity" || Instant::now() >= deadline {
            break got;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(got, "parity", "badge never converged to the real value");
}

/// Runs a `git` subcommand in `repo`, with a fixed identity so `commit` never fails on a
/// test box with no global `user.name`/`user.email`.
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
        ])
        .args(args)
        // Keep external Git from refreshing the index stat cache: tests that edit only
        // worktree files must leave the provider's HEAD/index fingerprint unchanged.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(repo)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn invalid_global_git_scope_config(name: &str) -> String {
    let field = if name == "git_counts" {
        "field = \"modified\"\n"
    } else {
        ""
    };
    format!(
        r#"
[daemon]
coalesce = "100ms"
config_check = "1h"

[badge.probe]
type = "command"
command = "echo"
args = ["loaded"]
interval = "60s"

[badge.invalid_git]
type = "builtin"
name = "{name}"
scope = "global"
{field}
"#
    )
}

fn command_counter_config(counter: &Path, interval: &str, pause: &str) -> String {
    format!(
        r#"
[daemon]
coalesce = "100ms"
config_check = "1h"

[badge.counted]
type = "command"
command = "sh"
args = ["-c", "printf x >> \"$1\"; sleep \"$2\"; printf x", "counter", "{counter}", "{pause}"]
interval = "{interval}"
timeout = "5s"
"#,
        counter = counter.display(),
    )
}

fn counter_len(path: &Path) -> usize {
    std::fs::read(path).map_or(0, |contents| contents.len())
}

/// Items 1+2: `git_branch` must be exactly as fresh as a real fs watcher (no stale-while-
/// revalidate window) and `git_status`/`git_counts` must invalidate on `git add` instead of
/// waiting out `interval`, both without an inotify thread.
#[test]
fn git_branch_and_status_are_fresh_immediately_after_a_repo_change() {
    let env = TestEnv::new(
        r#"
[badge.git_branch]
type = "builtin"
name = "git_branch"
interval = "5s"

[badge.git_status]
type = "builtin"
name = "git_status"
interval = "5s"
"#,
    );
    let repo = env.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("file.txt"), "one\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    // First call just spawns the daemon (no socket yet, so it returns empty); wait for it,
    // then prime both badges' caches for real.
    let _ = env.get_in(&["git_branch", "git_status"], &repo);
    wait_for_socket(&env);
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            env.get_in(&["git_branch", "git_status"], &repo).0
                == vec!["main".to_string(), String::new()]
        }),
        "badges never converged to their primed values"
    );

    // Both badges have a 5s interval, so converging well inside it proves the change was
    // seen without waiting it out. Not asserted on the very first `get`: the daemon's 20 ms
    // request budget (`cold_wait`) can expire on a loaded runner before the in-process read
    // or the `git status` refresh finishes, serving the previous value once.
    git(&repo, &["checkout", "-q", "-b", "br1"]);
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            env.get_in(&["git_branch"], &repo).0 == vec!["br1".to_string()]
        }),
        "git_branch was stale"
    );

    // git_status: staging a change touches `.git/index`, which the fingerprint check picks
    // up, refreshing the cached count instead of waiting out the 5s interval.
    std::fs::write(repo.join("file.txt"), "two\n").unwrap();
    git(&repo, &["add", "."]);
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            env.get_in(&["git_status"], &repo).0 == vec!["1".to_string()]
        }),
        "git_status was stale"
    );
}

/// Unstaged worktree edits do not change `.git/HEAD` or `.git/index`; git counts must still
/// refresh their cached porcelain result when the one-second badge TTL expires.
#[test]
fn git_counts_refresh_modified_and_untracked_after_worktree_only_edits() {
    let env = TestEnv::new(
        r#"
[daemon]
coalesce = "100ms"
config_check = "1h"

[badge.modified]
type = "builtin"
name = "git_counts"
field = "modified"
interval = "1s"

[badge.untracked]
type = "builtin"
name = "git_counts"
field = "untracked"
interval = "1s"
"#,
    );
    let repo = env.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("tracked.txt"), "before\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let _ = env.get_in(&["modified", "untracked"], &repo);
    wait_for_socket(&env);
    assert!(
        wait_until(Duration::from_secs(3), Duration::from_millis(20), || {
            let (lines, ok) = env.get_in(&["modified", "untracked"], &repo);
            let (status, status_ok) = env.run(&["status"]);
            let primed = ["modified", "untracked"].iter().all(|name| {
                status.lines().skip(1).any(|row| {
                    let mut columns = row.split_whitespace();
                    columns.next() == Some(*name) && columns.nth(1).is_some_and(|age| age != "-")
                })
            });
            ok && status_ok && lines == vec![String::new(), String::new()] && primed
        }),
        "git_counts did not prime to zero counts"
    );

    // These writes touch neither HEAD nor the index. The test's external Git setup commands
    // use GIT_OPTIONAL_LOCKS=0, and no Git command runs between these edits and the refresh.
    std::fs::write(repo.join("tracked.txt"), "after\n").unwrap();
    std::fs::write(repo.join("new.txt"), "untracked\n").unwrap();

    assert!(
        wait_until(Duration::from_secs(4), Duration::from_millis(25), || {
            let (lines, ok) = env.get_in(&["modified", "untracked"], &repo);
            ok && lines == vec!["1".to_string(), "1".to_string()]
        }),
        "modified and untracked counts stayed stale after their 1s TTL"
    );
}

/// Every git builtin is repo-scoped by definition. Rejecting `scope = "global"` must discard
/// the whole startup config rather than partially activating its otherwise-valid probe badge.
#[test]
fn global_scope_is_rejected_for_every_git_builtin_at_startup() {
    for name in [
        "git_branch",
        "git_status",
        "git_counts",
        "git_commit",
        "git_state",
        "git_stash",
    ] {
        let env = TestEnv::new(&invalid_global_git_scope_config(name));
        let _ = env.get(&["probe"]);
        wait_for_socket(&env);
        let _ = env.get(&["probe"]);

        let (status, ok) = env.run(&["status"]);
        assert!(ok, "status failed while checking {name}");
        let (errors, table): (Vec<_>, Vec<_>) = status
            .lines()
            .partition(|line| line.starts_with("config error: "));
        assert_eq!(
            table.len(),
            1,
            "{name} accepted scope = \"global\" at startup"
        );
        assert_eq!(errors.len(), 1, "{name}: startup config error not reported");
    }
}

/// A config containing a globally-scoped git builtin is invalid wholesale. An explicit
/// reload must leave the already-running badge config and cached value untouched.
#[test]
fn invalid_global_git_scope_reload_keeps_the_previous_config() {
    let env = TestEnv::new(
        r#"
[daemon]
coalesce = "100ms"
config_check = "1h"

[badge.stable]
type = "command"
command = "echo"
args = ["old"]
interval = "60s"
"#,
    );
    let _ = env.get(&["stable"]);
    wait_for_socket(&env);
    assert!(
        wait_until_value(&env, "stable", "old", Duration::from_secs(2)),
        "old config did not produce its initial value"
    );

    std::fs::write(
        env.config_path(),
        r#"
[daemon]
coalesce = "100ms"
config_check = "1h"

[badge.stable]
type = "command"
command = "echo"
args = ["new"]
interval = "60s"

[badge.invalid_git]
type = "builtin"
name = "git_branch"
scope = "global"
"#,
    )
    .unwrap();
    let (_, ok) = env.run(&["reload"]);
    assert!(ok, "reload request failed");

    let switched_to_invalid_config_value =
        wait_until(Duration::from_secs(2), Duration::from_millis(25), || {
            let (lines, ok) = env.get(&["stable"]);
            assert!(ok);
            lines.first().is_some_and(|line| line == "new")
        });
    assert!(
        !switched_to_invalid_config_value,
        "invalid reload replaced the old badge config"
    );
    assert_eq!(env.get(&["stable"]).0, vec!["old".to_string()]);
}

/// A reload-induced immediate fetch is not sufficient: after shortening 10s to 1s, the
/// scheduler must run again at the new interval, with no client get/status traffic.
#[test]
fn reload_shortens_interval_and_timer_refreshes_without_client_requests() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let initial_pause = format!("0.{:03}", nanos % 900 + 100);
    let env = TestEnv::new("");
    let counter = env.dir.join("runs");
    std::fs::write(
        env.config_path(),
        command_counter_config(&counter, "10s", &initial_pause),
    )
    .unwrap();

    let _ = env.get(&["counted"]);
    wait_for_socket(&env);
    let (lines, ok) = env.get(&["counted"]);
    assert!(ok);
    assert_eq!(lines.len(), 1);
    assert!(
        wait_until(Duration::from_secs(3), Duration::from_millis(20), || {
            counter_len(&counter) == 1
                && proc_cmdline_count(&initial_pause) == 0
                && env.get(&["counted"]).0 == vec!["x".to_string()]
        }),
        "initial counter command did not finish exactly once"
    );

    std::fs::write(
        env.config_path(),
        command_counter_config(&counter, "1s", "2"),
    )
    .unwrap();
    let (_, ok) = env.run(&["reload"]);
    assert!(ok, "reload request failed");
    assert!(
        wait_until(Duration::from_secs(3), Duration::from_millis(20), || {
            counter_len(&counter) >= 2
        }),
        "reload did not run the badge under the new config"
    );

    // Each command appends once at start, then sleeps for two seconds. Reset while that
    // reload fetch is still in flight so its immediate fetch cannot masquerade as a timer
    // refresh. From here on the test makes no client requests.
    std::fs::File::create(&counter).unwrap();
    assert_eq!(counter_len(&counter), 0);
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(20), || {
            counter_len(&counter) >= 1
        }),
        "the 1s timer did not refresh the badge without client requests"
    );
}

/// Explicit reload may block one filesystem worker on a FIFO, but the cached global hot
/// path, admin status, and daemon stop must remain live. Keep the writer open through stop
/// so shutdown proves it does not wait for the stuck worker.
#[test]
fn blocked_config_fifo_does_not_block_cached_global_get_status_or_stop() {
    let env = TestEnv::new(
        r#"
[daemon]
coalesce = "100ms"
config_check = "1h"

[badge.cached]
type = "command"
command = "echo"
args = ["warm"]
interval = "60s"
"#,
    );
    let _ = env.get(&["cached"]);
    wait_for_socket(&env);
    wait_for_daemon(&env);
    assert!(
        wait_until_value(&env, "cached", "warm", Duration::from_secs(2)),
        "global badge did not warm"
    );

    std::fs::remove_file(env.config_path()).unwrap();
    let mut fifo = FifoStall::create(&env.config_path());
    let reload = env.spawn(&["reload"]);
    fifo.wait_for_reader();

    let get_started = Instant::now();
    let (lines, ok) = env.get(&["cached"]);
    assert!(ok);
    assert_eq!(lines, vec!["warm".to_string()]);
    assert!(
        get_started.elapsed() < Duration::from_millis(450),
        "cached global get stalled behind FIFO config read"
    );

    let status_started = Instant::now();
    let (status, ok) = env.run(&["status"]);
    assert!(ok, "status failed during FIFO config read");
    assert!(
        status.contains("cached"),
        "status omitted the warm global key"
    );
    assert!(
        status_started.elapsed() < Duration::from_millis(450),
        "status stalled behind FIFO config read"
    );

    let reload_output = wait_for_child(reload, Duration::from_secs(2));
    assert!(
        reload_output.status.success(),
        "explicit reload client did not finish: {}",
        String::from_utf8_lossy(&reload_output.stderr)
    );
    let stop_started = Instant::now();
    let (_, ok) = env.run(&["stop"]);
    assert!(
        ok,
        "stop request failed while a filesystem worker was blocked"
    );
    assert!(
        stop_started.elapsed() < Duration::from_millis(450),
        "stop stalled behind FIFO config read"
    );
    wait_for_daemon_exit(&env);
    fifo.release();
}

/// Saturate all four bounded filesystem workers with in-process reads of FIFO `HEAD`s.
/// Global-only requests with that cwd must still return their cache entry, and stop must exit
/// while the readers remain blocked.
#[test]
fn blocked_git_head_reads_do_not_block_global_get_status_or_stop() {
    const WORKERS: usize = 4;

    let env = TestEnv::new(
        r#"
[daemon]
coalesce = "100ms"
config_check = "1h"

[badge.git_branch]
type = "builtin"
name = "git_branch"
interval = "5s"

[badge.global]
type = "command"
command = "echo"
args = ["cached"]
interval = "60s"
"#,
    );
    let _ = env.get(&["global"]);
    wait_for_socket(&env);
    wait_for_daemon(&env);
    assert!(
        wait_until_value(&env, "global", "cached", Duration::from_secs(2)),
        "global badge did not warm"
    );

    let mut repos = Vec::with_capacity(WORKERS);
    let mut fifos = Vec::with_capacity(WORKERS);
    for index in 0..WORKERS {
        let repo = env.dir.join(format!("repo-{index}"));
        let git_dir = repo.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        fifos.push(FifoStall::create(&git_dir.join("HEAD")));
        repos.push(repo);
    }

    // One request per repo, each parking one worker on its FIFO `HEAD`. The daemon skips a
    // read whose worker starts after the request's 20 ms budget (by design), so on a loaded
    // runner a repo may need another request before its FIFO has a reader. Wait long enough
    // per request that a started read is never doubled up on the same FIFO.
    for (repo, fifo) in repos.iter().zip(&mut fifos) {
        let attached = wait_until(Duration::from_secs(5), Duration::ZERO, || {
            let output = wait_for_child(
                env.spawn_get_in(&["git_branch"], repo),
                Duration::from_secs(2),
            );
            assert!(output.status.success());
            assert_eq!(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .collect::<Vec<_>>(),
                vec![""],
                "blocked git_branch request should return its empty fallback"
            );
            wait_until(Duration::from_secs(1), Duration::from_millis(5), || {
                fifo.try_attach_writer()
            })
        });
        assert!(attached, "no reader opened FIFO under {}", repo.display());
    }

    let get_started = Instant::now();
    let (lines, ok) = env.get_in(&["global"], &repos[0]);
    assert!(ok);
    assert_eq!(lines, vec!["cached".to_string()]);
    assert!(
        get_started.elapsed() < Duration::from_millis(450),
        "global get with a repo cwd stalled behind blocked git reads"
    );

    let status_started = Instant::now();
    let (status, ok) = env.run(&["status"]);
    assert!(
        ok,
        "status failed while all filesystem workers were blocked"
    );
    assert!(
        status.contains("global"),
        "status omitted the cached global key"
    );
    assert!(
        status_started.elapsed() < Duration::from_millis(450),
        "status stalled behind blocked git reads"
    );

    let stop_started = Instant::now();
    let (_, ok) = env.run(&["stop"]);
    assert!(ok, "stop failed while all filesystem workers were blocked");
    assert!(
        stop_started.elapsed() < Duration::from_millis(450),
        "stop stalled behind blocked git reads"
    );
    wait_for_daemon_exit(&env);
    for fifo in &mut fifos {
        fifo.release();
    }
}
