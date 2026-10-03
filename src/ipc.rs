//! Wire protocol/paths/config path for the daemon side, plus the JSON admin
//! (status/stop/reload) client. The `get` hot path (socket path resolution, non-blocking
//! connect, detached spawn, and its own text wire protocol) lives in `client.rs` and is
//! shared with the tiny `stfg` binary — nothing here duplicates it.

use std::io::{self, Write as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::client;

/// Cap on a single NDJSON line, either direction.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

pub const PROTOCOL_VERSION: u8 = 1;

/// Admin request (`status`/`stop`/`reload`); `get` uses `client.rs`'s text protocol.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub v: u8,
    pub cmd: String,
    /// The client's `CARGO_PKG_VERSION`. Informational only: the daemon's `handle_json` does
    /// not read it (binary-upgrade detection happens on the text `get`, see
    /// `handle_text_get`).
    #[serde(default)]
    pub ver: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub v: u8,
    pub ver: String,
    pub ok: bool,
    #[serde(default)]
    pub values: std::collections::BTreeMap<String, String>,
}

impl Response {
    pub fn ok(values: std::collections::BTreeMap<String, String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ver: env!("CARGO_PKG_VERSION").to_string(),
            ok: true,
            values,
        }
    }

    pub fn err() -> Self {
        Self {
            ok: false,
            ..Self::ok(std::collections::BTreeMap::new())
        }
    }
}

// ---- paths -----------------------------------------------------------------

/// Re-exported so daemon-side code (binds the socket, derives the lock path, vets the
/// runtime dir) and `client.rs` (connects to it) never disagree on where it is.
pub use crate::client::{runtime_dir, runtime_dir_is_private, socket_path};

pub fn lock_path() -> PathBuf {
    runtime_dir().join("daemon.lock")
}

/// `$STAR_FORGE_CONFIG`, else `$XDG_CONFIG_HOME/star-forge/config.toml`, else
/// `~/.config/star-forge/config.toml`.
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("STAR_FORGE_CONFIG")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("star-forge/config.toml");
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/star-forge/config.toml")
}

// ---- bounded line reader -----------------------------------------------------

/// Appends `chunk` up to its first `\n` to `buf`; `true` once the newline is seen (anything
/// after it is dropped). Errors as soon as the line body exceeds `cap` bytes, so a line of
/// exactly `cap` bytes plus `\n` is accepted and one more byte is not.
fn feed_line(buf: &mut Vec<u8>, chunk: &[u8], cap: usize) -> io::Result<bool> {
    let end = chunk.iter().position(|&b| b == b'\n');
    buf.extend_from_slice(&chunk[..end.unwrap_or(chunk.len())]);
    if buf.len() > cap {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    Ok(end.is_some())
}

/// Reads one `\n`-terminated line as raw bytes (discarding the newline), erroring if the
/// line is longer than `cap` bytes. EOF with no newline returns what was read so far.
/// Byte-oriented (rather than `String`) so callers can dispatch on the first byte (`{` JSON
/// vs `g` text `get`, see `daemon::handler::handle_connection`) before deciding how to decode the rest.
pub async fn read_line_bytes_capped_async<R>(r: &mut R, cap: usize) -> io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = r.read(&mut chunk).await?;
        if n == 0 || feed_line(&mut buf, &chunk[..n], cap)? {
            return Ok(buf);
        }
    }
}

/// Blocking [`read_line_bytes_capped_async`] returning UTF-8; used by the JSON admin client
/// to read the daemon's response.
pub fn read_line_capped<R: io::Read>(r: &mut R, cap: usize) -> io::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = r.read(&mut chunk)?;
        if n == 0 || feed_line(&mut buf, &chunk[..n], cap)? {
            return String::from_utf8(buf)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utf8"));
        }
    }
}

// ---- JSON admin client (status/stop/reload) ------------------------------------------

/// Writes the request and reads the response on an already-connected stream, within
/// `deadline`. `None` on any error or timeout; never touches the connect step, so callers
/// decide spawn-on-connect-failure semantics themselves.
fn exchange(
    stream: &mut std::os::unix::net::UnixStream,
    cmd: &str,
    deadline: Instant,
) -> Option<Response> {
    let req = Request {
        v: PROTOCOL_VERSION,
        cmd: cmd.to_string(),
        ver: env!("CARGO_PKG_VERSION").to_string(),
    };
    let mut line = serde_json::to_string(&req).ok()?;
    line.push('\n');

    client::tolerate_shut_socket(stream.set_write_timeout(Some(client::remaining(deadline)?)))
        .ok()?;
    stream.write_all(line.as_bytes()).ok()?;

    client::tolerate_shut_socket(stream.set_read_timeout(Some(client::remaining(deadline)?)))
        .ok()?;
    let resp_line = read_line_capped(stream, MAX_LINE_BYTES).ok()?;
    serde_json::from_str(&resp_line).ok()
}

/// Why an admin command got no response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminError {
    /// Nothing is listening on the socket.
    NotRunning,
    /// A daemon may be there, but the exchange failed (busy, timed out, bad reply).
    NoResponse,
}

/// `status`/`stop`/`reload`: administrative commands with their own (longer) timeout.
/// Never spawns a daemon. The hot `get` path has its own client entirely in `client.rs`.
pub fn client_admin(cmd: &str, timeout: Duration) -> Result<Response, AdminError> {
    let deadline = Instant::now() + timeout;
    match client::connect_daemon() {
        client::Connect::Ok(mut stream) => {
            exchange(&mut stream, cmd, deadline).ok_or(AdminError::NoResponse)
        }
        client::Connect::NotRunning => Err(AdminError::NotRunning),
        client::Connect::Busy => Err(AdminError::NoResponse),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reads_simple_line() {
        let mut c = Cursor::new(b"hello\nworld\n".to_vec());
        assert_eq!(read_line_capped(&mut c, 1024).unwrap(), "hello");
    }

    #[test]
    fn eof_without_newline_returns_partial() {
        let mut c = Cursor::new(b"partial".to_vec());
        assert_eq!(read_line_capped(&mut c, 1024).unwrap(), "partial");
    }

    #[test]
    fn oversized_line_errors() {
        let data = vec![b'a'; 200];
        let mut c = Cursor::new(data);
        assert!(read_line_capped(&mut c, 64).is_err());
    }

    #[test]
    fn empty_input_is_empty_line() {
        let mut c = Cursor::new(Vec::<u8>::new());
        assert_eq!(read_line_capped(&mut c, 1024).unwrap(), "");
    }

    #[tokio::test]
    async fn async_reads_simple_line_as_bytes() {
        let mut c = Cursor::new(b"hello\nworld\n".to_vec());
        assert_eq!(
            read_line_bytes_capped_async(&mut c, 1024).await.unwrap(),
            b"hello"
        );
    }

    #[test]
    fn cap_is_exact_for_both_readers() {
        let cap = 5000; // spans more than one 4 KiB chunk
        let mut ok = vec![b'a'; cap];
        ok.push(b'\n');
        let mut bad = vec![b'a'; cap + 1];
        bad.push(b'\n');
        assert_eq!(
            read_line_capped(&mut Cursor::new(ok.clone()), cap)
                .unwrap()
                .len(),
            cap
        );
        assert!(read_line_capped(&mut Cursor::new(bad.clone()), cap).is_err());
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(
            rt.block_on(read_line_bytes_capped_async(&mut Cursor::new(ok), cap))
                .unwrap()
                .len(),
            cap
        );
        assert!(
            rt.block_on(read_line_bytes_capped_async(&mut Cursor::new(bad), cap))
                .is_err()
        );
    }

    #[test]
    fn request_roundtrips_json() {
        let req = Request {
            v: 1,
            cmd: "status".into(),
            ver: "0.1.0".into(),
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back.cmd, "status");
        assert_eq!(back.ver, "0.1.0");
    }
}
