//! Shared `get` hot-path client: socket path resolution, non-blocking connect, detached
//! daemon spawn, the text wire protocol, and output. **std + libc only** — no
//! `tokio`/`serde`/`serde_json`/`regex`/`reqwest`/`toml` — so the tiny `stfg` binary
//! (`src/bin/stfg.rs`, which includes this file via `#[path]`) stays cheap to start and builds
//! with `--no-default-features`. `stfgd get` calls the same [`run_cli`]: exactly one
//! implementation backs both entry points.
//!
//! Wire protocol (see `docs/design.md` §2): request is one line, fields separated by
//! 0x1F (ASCII unit separator): `get<0x1F><version><0x1F><cwd>(<0x1F><badge>)*\n`, optionally
//! followed by `<0x1F>ENV_MARK(<0x1F>NAME=value)*` (the client's environment).
//! Response is exactly one line per requested badge, in order, then EOF.

use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Cap on the whole response read (matches the daemon's own line caps).
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Field that ends the badge list and starts the env block: `NAME=value` fields, the client's
/// whole environment (the daemon filters per badge, so the client never reads the config).
///
/// A request without it parses as before (daemon env). An older daemon treats the block as
/// extra badge names: unknown, empty, and cut off by the client's line count.
pub const ENV_MARK: &str = "\u{1e}env";

/// Per-variable cap (name + value): skips exported shell functions, `LS_COLORS` and the like
/// so one huge entry doesn't cost the whole env block.
const MAX_ENV_ENTRY_BYTES: usize = 4 * 1024;

/// Cap on the request line (the daemon's `ipc::MAX_LINE_BYTES`, kept below it); an
/// environment that wouldn't fit is left out (daemon env used) rather than failing the request.
const MAX_REQUEST_BYTES: usize = 60 * 1024;

/// Separates fields within the one-line request; also forbidden inside cwd/badge values.
const FIELD_SEP: char = '\u{1f}';

// ---- paths -----------------------------------------------------------------

fn uid() -> libc::uid_t {
    // SAFETY: getuid(2) has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// `$XDG_RUNTIME_DIR/star-forge` or `/tmp/star-forge-$UID`.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("star-forge");
    }
    PathBuf::from(format!("/tmp/star-forge-{}", uid()))
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("sock")
}

/// Whether the runtime dir `dir` can be trusted with the socket: a real directory (not a
/// symlink) owned by this user that no one else can write to, so no one else can have put
/// a socket (or the lock) in it.
///
/// The `/tmp/star-forge-$UID` fallback sits in world-writable
/// `/tmp`, where anyone can pre-create it — and on macOS, where `XDG_RUNTIME_DIR` is
/// normally unset, that fallback is the default. `Err` is the `lstat(2)` failure
/// (`NotFound`: no daemon has created it yet).
pub fn runtime_dir_is_private(dir: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::symlink_metadata(dir)?;
    Ok(meta.is_dir() && meta.uid() == uid() && meta.mode() & 0o022 == 0)
}

/// Connects to the daemon at `socket_path()` without blocking, once its runtime dir passes
/// [`runtime_dir_is_private`].
///
/// A missing dir is `NotRunning` without connecting at all
/// (whatever appears there after the check is unvetted); an untrusted one is `Busy`, so the
/// client neither talks to it nor spawns a daemon that would refuse it anyway.
pub fn connect_daemon() -> Connect {
    let path = socket_path();
    let Some(dir) = path.parent() else {
        return Connect::Busy;
    };
    match runtime_dir_is_private(dir) {
        Ok(true) => connect_nonblocking(&path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Connect::NotRunning,
        _ => Connect::Busy,
    }
}

// ---- non-blocking connect ---------------------------------------------------------

/// Outcome of a non-blocking connect attempt at the daemon socket.
pub enum Connect {
    Ok(UnixStream),
    /// No runtime dir yet, or `ENOENT`/`ECONNREFUSED`: nothing is listening (no daemon, or
    /// a stale socket left behind by one that died — or, on macOS, a full backlog). Safe to
    /// spawn a new daemon: a redundant one loses the `flock` and exits.
    NotRunning,
    /// Anything else, including a full listen backlog on Linux (`EAGAIN`, immediately —
    /// `AF_UNIX` has no handshake to report `EINPROGRESS` for) and an untrusted runtime dir
    /// (see [`connect_daemon`]). The daemon may be alive but busy, or something else is
    /// wrong either way; never spawn a second daemon on top of it.
    Busy,
}

/// A fresh non-blocking, close-on-exec `AF_UNIX` stream socket (Linux: one `socket(2)`).
#[cfg(target_os = "linux")]
fn new_stream_socket() -> Option<OwnedFd> {
    let kind = libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    // SAFETY: AF_UNIX/`kind`/0 are valid, static arguments; the return value is checked
    // before the fd is used for anything.
    let fd = unsafe { libc::socket(libc::AF_UNIX, kind, 0) };
    // SAFETY: `fd` was just returned by the successful `socket(2)` call and isn't owned
    // anywhere else.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A fresh non-blocking, close-on-exec `AF_UNIX` stream socket. No
/// `SOCK_NONBLOCK`/`SOCK_CLOEXEC` outside Linux: set both after `socket(2)` instead. Plain
/// `F_SETFL` (no `F_GETFL` to OR into) is exact: a fresh socket has no other status flags.
#[cfg(not(target_os = "linux"))]
fn new_stream_socket() -> Option<OwnedFd> {
    // SAFETY: AF_UNIX/SOCK_STREAM/0 are valid, static arguments; the return value is
    // checked before the fd is used for anything.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` was just returned by the successful `socket(2)` call and isn't owned
    // anywhere else.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: `fd` is valid; F_SETFD/F_SETFL on it are plain flag operations.
    let failed = unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) < 0
            || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) < 0
    };
    (!failed).then_some(fd)
}

/// Connects to `path` without ever blocking the caller. A blocking `connect()` to an
/// `AF_UNIX` socket blocks when the listener's backlog is full (unlike TCP, there's no
/// handshake to queue behind), which would blow through the client's deadline; a
/// non-blocking socket reports a full backlog as an immediate `EAGAIN` instead. (macOS
/// reports a full backlog as `ECONNREFUSED`, so there it reads as `NotRunning`: the spawned
/// daemon loses the `flock` race and exits, costing one wasted spawn under overload.)
fn connect_nonblocking(path: &Path) -> Connect {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: zero-initializing a plain-data C struct is always valid; every field is
    // either left zeroed or set below before use.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    // sun_path is 108 bytes on Linux, 104 on macOS, and must hold a trailing NUL.
    if bytes.len() >= addr.sun_path.len() {
        return Connect::Busy;
    }
    addr.sun_family = libc::sa_family_t::try_from(libc::AF_UNIX).unwrap_or_default();
    for (dst, &src) in addr.sun_path.iter_mut().zip(bytes) {
        // `c_char` is `i8` on x86_64/Apple, `u8` on Linux aarch64: a byte copy either way.
        *dst = libc::c_char::from_ne_bytes([src]);
    }
    // `sun_len` (BSD/macOS only) stays 0: the kernel sets it from `addr_len`.
    let Ok(addr_len) = libc::socklen_t::try_from(
        std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1,
    ) else {
        return Connect::Busy;
    };

    let Some(fd) = new_stream_socket() else {
        return Connect::Busy;
    };

    // SAFETY: `fd` is a valid, just-created socket; `addr` is a fully initialized
    // `sockaddr_un` and `addr_len` is exactly its used length.
    let rc = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
            addr_len,
        )
    };
    if rc != 0 {
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOENT | libc::ECONNREFUSED) => Connect::NotRunning,
            _ => Connect::Busy,
        };
    }

    // Connected immediately (AF_UNIX has no EINPROGRESS handshake to wait out): clear
    // O_NONBLOCK (one `ioctl(FIONBIO)`) so the caller's read/write timeouts apply from here
    // on; a stream left non-blocking would fail its first read with `WouldBlock` anyway.
    let stream = UnixStream::from(fd);
    if stream.set_nonblocking(false).is_err() {
        return Connect::Busy;
    }
    Connect::Ok(stream)
}

// ---- detached daemon spawn ---------------------------------------------------

/// Tries to spawn `<prog> daemon`, fully detached: null stdio, not waited on, deliberately
/// **no** `process_group(0)` (a group leader can't `setsid()`; the daemon does that
/// itself). Returns whether the spawn succeeded.
fn try_spawn_daemon(prog: &Path) -> bool {
    Command::new(prog)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// Spawns the daemon detached: the `stfgd` binary next to `current_exe()` (same dir —
/// works whether the caller is `stfgd` or the tiny `stfg`), falling back to `stfgd`
/// resolved from `PATH` if that doesn't exist.
fn spawn_daemon_detached() {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && try_spawn_daemon(&dir.join("stfgd"))
    {
        return;
    }
    try_spawn_daemon(Path::new("stfgd"));
}

// ---- wire protocol -----------------------------------------------------------

/// A field may not contain `\n` (would break line framing) or the field separator.
fn valid_field(s: &str) -> bool {
    !s.chars().any(|c| c == '\n' || c == FIELD_SEP)
}

/// Encodes the one-line `get` request. `None` if `cwd` or any badge contains `\n` or the
/// field separator — the caller must print empty lines without connecting in that case.
fn encode_request(
    version: &str,
    cwd: &str,
    badges: &[String],
    env: &[(String, String)],
) -> Option<String> {
    if !valid_field(cwd) || badges.iter().any(|b| !valid_field(b)) {
        return None;
    }
    let mut req = String::with_capacity(
        8 + version.len() + cwd.len() + badges.iter().map(|b| b.len() + 1).sum::<usize>(),
    );
    req.push_str("get");
    req.push(FIELD_SEP);
    req.push_str(version);
    req.push(FIELD_SEP);
    req.push_str(cwd);
    for b in badges {
        req.push(FIELD_SEP);
        req.push_str(b);
    }
    let base = req.len();
    let mut marked = false;
    for (k, v) in env {
        if k.contains('=')
            || k.len() + v.len() > MAX_ENV_ENTRY_BYTES
            || !valid_field(k)
            || !valid_field(v)
        {
            continue;
        }
        if !marked {
            req.push(FIELD_SEP);
            req.push_str(ENV_MARK);
            marked = true;
        }
        req.push(FIELD_SEP);
        req.push_str(k);
        req.push('=');
        req.push_str(v);
    }
    if req.len() > MAX_REQUEST_BYTES {
        req.truncate(base);
    }
    req.push('\n');
    Some(req)
}

/// Splits the response body into exactly `n` lines: missing trailing lines become empty
/// (any failure/short read), extra lines are ignored.
fn parse_response(body: &str, n: usize) -> Vec<String> {
    let mut out: Vec<String> = body.lines().map(str::to_string).collect();
    out.resize(n, String::new());
    out.truncate(n);
    out
}

/// Socket operations in the hot path use the remaining whole-request budget, not a fresh
/// per-operation timeout.
trait DeadlineStream: io::Read + io::Write {
    fn set_read_timeout(&mut self, timeout: Duration) -> io::Result<()>;
    fn set_write_timeout(&mut self, timeout: Duration) -> io::Result<()>;
}

impl DeadlineStream for UnixStream {
    fn set_read_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        tolerate_shut_socket(Self::set_read_timeout(self, Some(timeout)))
    }

    fn set_write_timeout(&mut self, timeout: Duration) -> io::Result<()> {
        tolerate_shut_socket(Self::set_write_timeout(self, Some(timeout)))
    }
}

/// Result of `SO_RCVTIMEO`/`SO_SNDTIMEO` on a connected stream, treating a shut-down socket
/// as success. XNU (macOS) fails `setsockopt` with `EINVAL` once a socket can neither send
/// nor receive, which is the normal state right after the daemon writes its reply and
/// closes. Such a socket cannot block: reads return what's buffered and then EOF, writes
/// fail at once, so there is nothing left to bound. Linux never reports this.
#[doc(hidden)]
pub fn tolerate_shut_socket(result: io::Result<()>) -> io::Result<()> {
    const TOLERATE_EINVAL: bool = cfg!(not(target_os = "linux"));
    match result {
        Err(err) if TOLERATE_EINVAL && err.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        other => other,
    }
}

fn deadline_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "client deadline exceeded")
}

/// Writes all bytes, refreshing the socket timeout before each potentially blocking write.
fn write_all_before_deadline<S: DeadlineStream>(
    stream: &mut S,
    mut bytes: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        let timeout = remaining(deadline).ok_or_else(deadline_error)?;
        stream.set_write_timeout(timeout)?;
        let result = stream.write(bytes);
        if remaining(deadline).is_none() {
            return Err(deadline_error());
        }
        match result {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write request",
                ));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Reads until EOF or `lines` newlines have been seen (the daemon's reply is exactly one
/// `\n`-terminated line per badge, embedded newlines replaced, so the last `\n` ends it and
/// waiting for EOF costs an extra `read`/`setsockopt`), capped at `cap` bytes and bounded by
/// one absolute deadline.
fn read_all_capped_before_deadline<S: DeadlineStream>(
    stream: &mut S,
    lines: usize,
    cap: usize,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    if lines == 0 {
        return Ok(Vec::new());
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut seen = 0;
    loop {
        let timeout = remaining(deadline).ok_or_else(deadline_error)?;
        stream.set_read_timeout(timeout)?;
        let result = stream.read(&mut chunk);
        if remaining(deadline).is_none() {
            return Err(deadline_error());
        }
        let n = match result {
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response too long",
            ));
        }
        seen += chunk[..n]
            .iter()
            .map(|&b| usize::from(b == b'\n'))
            .sum::<usize>();
        if seen >= lines {
            break;
        }
    }
    Ok(buf)
}

#[doc(hidden)]
pub fn remaining(deadline: Instant) -> Option<Duration> {
    let now = Instant::now();
    (deadline > now).then(|| deadline - now)
}

/// Bound filesystem-dependent preparation without linking an async runtime. Timed-out
/// workers are not joined; the short-lived client process terminates them on exit.
fn run_before_deadline<T: Send + 'static>(
    deadline: Instant,
    job: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    remaining(deadline)?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .spawn(move || {
            if remaining(deadline).is_some() {
                let _ = sender.send(job());
            }
        })
        .ok()?;
    let result = receiver.recv_timeout(remaining(deadline)?).ok()?;
    remaining(deadline)?;
    Some(result)
}

// ---- the hot path ---------------------------------------------------------------

/// The process cwd as sent on the wire; empty if it can't be determined.
fn current_dir_string() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// The process environment (UTF-8 entries only), sent so the daemon can run `env`-forwarding
/// badges with it.
fn current_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}

/// Parses `get`'s argv (`<badge>... [--cwd <path>]`), shared by `stfg` and `stfgd get`.
fn parse_get_args(mut args: impl Iterator<Item = String>) -> (Vec<String>, Option<String>) {
    let mut badges = Vec::new();
    let mut cwd = None;
    while let Some(arg) = args.next() {
        if arg == "--cwd" {
            cwd = args.next();
        } else {
            badges.push(arg);
        }
    }
    (badges, cwd)
}

/// The cwd to send: `cwd` if given, else the process cwd so path-scoped (git) badges work
/// without an explicit `--cwd`, matching how starship invokes `get <badge>` in the prompt's
/// dir. No canonicalize. Inline on Linux: `getcwd(2)` reads the dentry cache, not the
/// filesystem, so it can't hang on a dead mount.
#[cfg(target_os = "linux")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "same signature as the non-Linux body"
)]
fn initial_cwd(cwd: Option<&str>, _deadline: Instant) -> Option<String> {
    Some(cwd.map_or_else(current_dir_string, str::to_string))
}

/// Elsewhere (macOS `getcwd(3)` may `open(".")` and ask the filesystem) the process cwd is
/// read under the deadline instead.
#[cfg(not(target_os = "linux"))]
fn initial_cwd(cwd: Option<&str>, deadline: Instant) -> Option<String> {
    cwd.map_or_else(
        || run_before_deadline(deadline, current_dir_string),
        |cwd| Some(cwd.to_string()),
    )
}

/// Client deadline unless `STAR_FORGE_TIMEOUT_MS` overrides it; `daemon.cold_wait` must stay under it.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(30);

/// `stfgd get`/`stfg` hot path: always returns exactly `badges.len()`
/// strings, empty on any failure, never panics, never writes to stderr. Only a missing
/// runtime dir or `ENOENT`/`ECONNREFUSED` (no daemon, stale socket) spawns a new one; a
/// busy (full backlog on Linux) or unresponsive daemon, or an untrusted runtime dir, does
/// not, per the design.
fn client_get(badges: &[String], cwd: Option<&str>) -> Vec<String> {
    let timeout = std::env::var("STAR_FORGE_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
    let deadline = Instant::now() + timeout;
    let empty = || vec![String::new(); badges.len()];

    let Some(cwd) = initial_cwd(cwd, deadline) else {
        return empty();
    };
    let Some(req) = encode_request(env!("CARGO_PKG_VERSION"), &cwd, badges, &current_env()) else {
        return empty();
    };

    let mut stream = match connect_daemon() {
        Connect::Ok(stream) => stream,
        // The client never waits for daemon startup: spawn and answer empty now.
        Connect::NotRunning => {
            run_before_deadline(deadline, spawn_daemon_detached);
            return empty();
        }
        Connect::Busy => return empty(),
    };

    if write_all_before_deadline(&mut stream, req.as_bytes(), deadline).is_err() {
        return empty();
    }

    read_all_capped_before_deadline(&mut stream, badges.len(), MAX_RESPONSE_BYTES, deadline)
        .map_or_else(
            |_| empty(),
            |buf| {
                String::from_utf8(buf)
                    .map_or_else(|_| empty(), |body| parse_response(&body, badges.len()))
            },
        )
}

/// Writes one line per value to stdout in a single `write_all`, ignoring errors (e.g.
/// `EPIPE` from a closed pipe: Rust ignores `SIGPIPE`) — the hot path always exits 0 and
/// never touches stderr.
fn print_values(values: &[String]) {
    let mut buf = String::new();
    for v in values {
        buf.push_str(v);
        buf.push('\n');
    }
    let _ = io::stdout().lock().write_all(buf.as_bytes());
}

/// CLI entry shared by `stfg` and `stfgd get`: `<badge>... [--cwd <path>]`, prints one line
/// per badge, always succeeds.
pub fn run_cli(args: impl Iterator<Item = String>) {
    let (badges, cwd) = parse_get_args(args);
    let values = client_get(&badges, cwd.as_deref());
    print_values(&values);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    enum ReadAction {
        Bytes { bytes: Vec<u8>, delay: Duration },
        Eof { delay: Duration },
        Stall,
    }

    struct WriteAction {
        max_bytes: usize,
        delay: Duration,
    }

    #[derive(Default)]
    struct FakeDeadlineStream {
        reads: VecDeque<ReadAction>,
        writes: VecDeque<WriteAction>,
        read_timeouts: Vec<Duration>,
        write_timeouts: Vec<Duration>,
        active_read_timeout: Option<Duration>,
        written: Vec<u8>,
    }

    impl FakeDeadlineStream {
        fn with_reads(reads: impl IntoIterator<Item = ReadAction>) -> Self {
            Self {
                reads: reads.into_iter().collect(),
                ..Self::default()
            }
        }
    }

    impl io::Read for FakeDeadlineStream {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            match self.reads.pop_front().unwrap_or(ReadAction::Eof {
                delay: Duration::ZERO,
            }) {
                ReadAction::Bytes { bytes, delay } => {
                    std::thread::sleep(delay);
                    let n = bytes.len().min(buffer.len());
                    buffer[..n].copy_from_slice(&bytes[..n]);
                    if n < bytes.len() {
                        self.reads.push_front(ReadAction::Bytes {
                            bytes: bytes[n..].to_vec(),
                            delay: Duration::ZERO,
                        });
                    }
                    Ok(n)
                }
                ReadAction::Eof { delay } => {
                    std::thread::sleep(delay);
                    Ok(0)
                }
                ReadAction::Stall => {
                    std::thread::sleep(
                        self.active_read_timeout
                            .expect("the deadline helper must set a read timeout"),
                    );
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "fake stream stalled",
                    ))
                }
            }
        }
    }

    impl io::Write for FakeDeadlineStream {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let (max_bytes, delay) = self
                .writes
                .pop_front()
                .map_or((buffer.len(), Duration::ZERO), |action| {
                    (action.max_bytes, action.delay)
                });
            std::thread::sleep(delay);
            let n = max_bytes.min(buffer.len());
            self.written.extend_from_slice(&buffer[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl DeadlineStream for FakeDeadlineStream {
        fn set_read_timeout(&mut self, timeout: Duration) -> io::Result<()> {
            self.active_read_timeout = Some(timeout);
            self.read_timeouts.push(timeout);
            Ok(())
        }

        fn set_write_timeout(&mut self, timeout: Duration) -> io::Result<()> {
            self.write_timeouts.push(timeout);
            Ok(())
        }
    }

    #[test]
    fn encode_request_joins_fields_with_unit_separator() {
        let badges = vec!["git_branch".to_string(), "git_status".to_string()];
        let req = encode_request("0.1.0", "/home/u/proj", &badges, &[]).unwrap();
        assert_eq!(
            req,
            "get\u{1f}0.1.0\u{1f}/home/u/proj\u{1f}git_branch\u{1f}git_status\n"
        );
    }

    #[test]
    fn encode_request_handles_no_badges() {
        let req = encode_request("0.1.0", "/tmp", &[], &[]).unwrap();
        assert_eq!(req, "get\u{1f}0.1.0\u{1f}/tmp\n");
    }

    #[test]
    fn encode_request_appends_env_block_after_badges() {
        let env = [
            ("PATH".to_string(), "/a:/b".to_string()),
            ("BAD".to_string(), "x\ny".to_string()),
            ("A=B".to_string(), "v".to_string()),
            ("FOO".to_string(), "bar=baz".to_string()),
        ];
        let req = encode_request("0.1.0", "/tmp", &["n".to_string()], &env).unwrap();
        assert_eq!(
            req,
            "get\u{1f}0.1.0\u{1f}/tmp\u{1f}n\u{1f}\u{1e}env\u{1f}PATH=/a:/b\u{1f}FOO=bar=baz\n"
        );
    }

    #[test]
    fn encode_request_skips_oversized_entries_and_drops_an_oversized_block() {
        let big = "x".repeat(MAX_ENV_ENTRY_BYTES);
        let env = [
            ("BIG".to_string(), big),
            ("PATH".to_string(), "/a".to_string()),
        ];
        let req = encode_request("0.1.0", "/tmp", &[], &env).unwrap();
        assert_eq!(
            req,
            "get\u{1f}0.1.0\u{1f}/tmp\u{1f}\u{1e}env\u{1f}PATH=/a\n"
        );

        // Many individually fine entries that together exceed the line cap: block dropped.
        let many: Vec<_> = (0..40)
            .map(|i| (format!("V{i}"), "y".repeat(MAX_ENV_ENTRY_BYTES - 8)))
            .collect();
        let req = encode_request("0.1.0", "/tmp", &[], &many).unwrap();
        assert_eq!(req, "get\u{1f}0.1.0\u{1f}/tmp\n");
    }

    #[test]
    fn encode_request_rejects_newline_in_cwd() {
        assert!(encode_request("0.1.0", "/tmp/evil\ninjected", &[], &[]).is_none());
    }

    #[test]
    fn encode_request_rejects_field_separator_in_badge() {
        let badges = vec!["a\u{1f}b".to_string()];
        assert!(encode_request("0.1.0", "/tmp", &badges, &[]).is_none());
    }

    #[test]
    fn encode_request_rejects_newline_in_badge() {
        let badges = vec!["a\nb".to_string()];
        assert!(encode_request("0.1.0", "/tmp", &badges, &[]).is_none());
    }

    #[test]
    fn parse_response_splits_lines_in_order() {
        let out = parse_response("main\n\nclean", 3);
        assert_eq!(
            out,
            vec!["main".to_string(), String::new(), "clean".to_string()]
        );
    }

    #[test]
    fn parse_response_pads_missing_trailing_lines() {
        let out = parse_response("only-one", 3);
        assert_eq!(
            out,
            vec!["only-one".to_string(), String::new(), String::new()]
        );
    }

    #[test]
    fn parse_response_truncates_extra_lines() {
        let out = parse_response("a\nb\nc\nd", 2);
        assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parse_response_empty_body_is_all_empty() {
        let out = parse_response("", 2);
        assert_eq!(out, vec![String::new(), String::new()]);
    }

    #[test]
    fn trickled_response_cannot_extend_absolute_read_deadline() {
        let response = b"main\nok\n";
        let mut reads: Vec<_> = response
            .iter()
            .map(|byte| ReadAction::Bytes {
                bytes: vec![*byte],
                delay: Duration::from_millis(6),
            })
            .collect();
        reads.push(ReadAction::Eof {
            delay: Duration::ZERO,
        });
        let mut stream = FakeDeadlineStream::with_reads(reads);

        let err = read_all_capped_before_deadline(
            &mut stream,
            usize::MAX,
            MAX_RESPONSE_BYTES,
            Instant::now() + Duration::from_millis(40),
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            stream
                .read_timeouts
                .windows(2)
                .all(|timeouts| timeouts[1] <= timeouts[0]),
            "each read should use no more than the previous remaining budget"
        );
        assert!(!stream.reads.is_empty(), "the trickle must be cut off");
    }

    /// The daemon writes its reply and closes before the client reads; on macOS the
    /// per-read `setsockopt` then fails with `EINVAL`, which must not discard the reply.
    #[test]
    fn reply_is_read_after_the_peer_already_closed() {
        let (mut daemon, mut client) = UnixStream::pair().unwrap();
        io::Write::write_all(&mut daemon, b"main\nok\n").unwrap();
        drop(daemon);

        let body = read_all_capped_before_deadline(
            &mut client,
            usize::MAX,
            MAX_RESPONSE_BYTES,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(body, b"main\nok\n");
    }

    #[test]
    fn reply_returns_after_the_expected_lines_without_waiting_for_eof() {
        let (mut daemon, mut client) = UnixStream::pair().unwrap();
        io::Write::write_all(&mut daemon, b"main\nok\n").unwrap();

        // `daemon` stays open: only the line count can end this read.
        let body = read_all_capped_before_deadline(
            &mut client,
            2,
            MAX_RESPONSE_BYTES,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();

        assert_eq!(body, b"main\nok\n");
        drop(daemon);
    }

    #[test]
    fn stalled_response_times_out() {
        let mut stream = FakeDeadlineStream::with_reads([ReadAction::Stall]);

        let err = read_all_capped_before_deadline(
            &mut stream,
            usize::MAX,
            MAX_RESPONSE_BYTES,
            Instant::now() + Duration::from_millis(20),
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(stream.read_timeouts.len(), 1);
    }

    #[test]
    fn normal_response_completes_before_deadline() {
        let body = b"main\nclean\n";
        let mut stream = FakeDeadlineStream::with_reads([
            ReadAction::Bytes {
                bytes: body.to_vec(),
                delay: Duration::from_millis(1),
            },
            ReadAction::Eof {
                delay: Duration::ZERO,
            },
        ]);

        let response = read_all_capped_before_deadline(
            &mut stream,
            usize::MAX,
            MAX_RESPONSE_BYTES,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(response, body);
        assert_eq!(
            parse_response(std::str::from_utf8(&response).unwrap(), 2),
            vec!["main".to_string(), "clean".to_string()]
        );
        assert_eq!(stream.read_timeouts.len(), 2);
    }

    #[test]
    fn partial_request_writes_share_the_absolute_deadline() {
        let mut stream = FakeDeadlineStream {
            writes: (0..8)
                .map(|_| WriteAction {
                    max_bytes: 1,
                    delay: Duration::from_millis(6),
                })
                .collect(),
            ..FakeDeadlineStream::default()
        };

        let err = write_all_before_deadline(
            &mut stream,
            b"request",
            Instant::now() + Duration::from_millis(25),
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(stream.written.len() < b"request".len());
    }

    #[test]
    fn run_before_deadline_returns_a_prompt_result() {
        assert_eq!(
            run_before_deadline(Instant::now() + Duration::from_secs(1), || 7),
            Some(7)
        );
    }

    #[test]
    fn run_before_deadline_skips_work_after_the_deadline() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        let result = run_before_deadline(Instant::now(), move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(result.is_none());
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn run_before_deadline_does_not_join_blocked_work() {
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let start = Instant::now();
        let result = run_before_deadline(start + Duration::from_millis(20), move || {
            let _ = gate.recv();
        });
        let elapsed = start.elapsed();
        release.send(()).unwrap();
        assert!(result.is_none());
        assert!(elapsed < Duration::from_secs(1), "waited {elapsed:?}");
    }

    #[test]
    fn run_before_deadline_fails_soft_on_panicking_work() {
        let result = run_before_deadline(Instant::now() + Duration::from_secs(1), || -> u8 {
            panic!("worker failure is reported as no result")
        });
        assert!(result.is_none());
    }

    #[test]
    fn nonblocking_connect_reports_notrunning_on_missing_socket() {
        let dir = crate::test_support::TempDir::new("client-missing");
        let sock = dir.path().join("does-not-exist");
        assert!(matches!(connect_nonblocking(&sock), Connect::NotRunning));
    }

    #[test]
    fn runtime_dir_must_be_a_real_dir_no_one_else_can_write() {
        use std::os::unix::fs::PermissionsExt as _;
        let set_mode = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let root_tmp = crate::test_support::TempDir::new("client-private");
        let root = root_tmp.path().to_path_buf();
        let dir = root.join("star-forge");
        let missing = runtime_dir_is_private(&dir).unwrap_err().kind();

        std::fs::create_dir_all(&dir).unwrap();
        set_mode(&dir, 0o700);
        let private = runtime_dir_is_private(&dir).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        let symlinked = runtime_dir_is_private(&link).unwrap();
        set_mode(&dir, 0o755);
        let readable = runtime_dir_is_private(&dir).unwrap();
        set_mode(&dir, 0o777);
        let writable = runtime_dir_is_private(&dir).unwrap();

        assert_eq!(missing, io::ErrorKind::NotFound);
        assert!(private, "an owned 0700 dir is trusted");
        assert!(!symlinked, "a symlink is never trusted");
        assert!(readable, "group/other read can't plant a socket");
        assert!(!writable, "a dir others can write to is never trusted");
    }

    /// What a full backlog reports: an immediate `EAGAIN` on Linux (`Busy`, no spawn);
    /// macOS refuses with `ECONNREFUSED`, indistinguishable from a stale socket.
    const fn is_full_backlog(outcome: &Connect) -> bool {
        #[cfg(target_os = "linux")]
        let full = matches!(outcome, Connect::Busy);
        #[cfg(not(target_os = "linux"))]
        let full = matches!(outcome, Connect::NotRunning);
        full
    }

    /// MUST fix: a blocking `connect()` to an `AF_UNIX` socket blocks the caller when the
    /// listener's backlog is full instead of failing (Linux), which would blow through the
    /// client's deadline. This fills a real backlog (using the same non-blocking helper,
    /// so the filling itself can never hang either) and checks that once it's full, an
    /// extra connect attempt fails immediately with the platform's full-backlog outcome.
    // `holders` is never read: its only job is to keep every connected stream's fd open
    // so the backlog stays full for the rest of the test, not to be inspected.
    #[allow(clippy::collection_is_never_read)]
    #[test]
    fn nonblocking_connect_fails_fast_without_blocking_on_full_backlog() {
        let dir = crate::test_support::TempDir::new("client-backlog");
        let sock = dir.path().join("sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        // std listens with a large backlog (`net.core.somaxconn` on Linux), which can exceed
        // the fd limit; `EMFILE` would then masquerade as a full backlog. Shrink it to 1 so a
        // couple of connects fill it. Never accept(): every connection stays queued.
        // SAFETY: `listener` owns a valid listening socket for the duration of the call.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);

        let mut holders = Vec::new();
        let mut saw_full = false;
        let fill_start = Instant::now();
        for _ in 0..64 {
            match connect_nonblocking(&sock) {
                Connect::Ok(s) => holders.push(s),
                other => {
                    assert!(is_full_backlog(&other), "unexpected outcome while filling");
                    saw_full = true;
                    break;
                }
            }
        }
        let fill_elapsed = fill_start.elapsed();
        assert!(
            fill_elapsed < Duration::from_secs(2),
            "filling the backlog took {fill_elapsed:?}; a non-blocking connect must never block"
        );
        assert!(saw_full, "a backlog of 1 never filled within 64 connects");

        let deadline_start = Instant::now();
        let outcome = connect_nonblocking(&sock);
        let elapsed = deadline_start.elapsed();
        drop(listener);

        assert!(
            elapsed < Duration::from_millis(100),
            "connect on a full backlog took {elapsed:?}, should return immediately"
        );
        assert!(
            is_full_backlog(&outcome),
            "expected the full-backlog outcome"
        );
    }
}
