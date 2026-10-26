//! Wire protocol: connection dispatch, the JSON admin commands and the plain-text `get`
//! (parsing, line framing, the post-answer upgrade check).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt as _;
use tokio::net::UnixStream;

use super::reload::{ReloadCheck, check_reload, schedule_reload_check};
use super::render::{build_status, response_lines};
use super::resolve::{ClientEnv, handle_get};
use super::upgrade::{Exe, Verdict};
use super::{Daemon, MAINTENANCE_BUDGET};
use crate::ipc;

/// Reads one request line and dispatches on its first byte: `{` is JSON (admin commands),
/// `g` is the plain-text `get` protocol (see `client.rs`). Anything else (including an
/// empty line) drops the connection.
pub(super) async fn handle_connection(state: Arc<Daemon>, mut stream: UnixStream) {
    let now = Instant::now();
    state.last_request.set(now);

    let Ok(Ok(line)) = tokio::time::timeout(
        Duration::from_secs(2),
        ipc::read_line_bytes_capped_async(&mut stream, ipc::MAX_LINE_BYTES),
    )
    .await
    else {
        return;
    };

    match line.first() {
        Some(b'{') => handle_json(&state, &mut stream, &line, now).await,
        Some(b'g') => handle_text_get(&state, &mut stream, &line, now).await,
        _ => {}
    }
}

/// JSON admin commands: `status`/`stop`/`reload`. `get` no longer goes through JSON (see
/// `handle_text_get`).
async fn handle_json(state: &Arc<Daemon>, stream: &mut UnixStream, line: &[u8], now: Instant) {
    let Ok(text) = std::str::from_utf8(line) else {
        return;
    };
    let Ok(req) = serde_json::from_str::<ipc::Request>(text) else {
        return;
    };

    if req.cmd == "reload" {
        check_reload(state, ReloadCheck::Forced, now).await;
    } else {
        schedule_reload_check(state, now);
    }

    let resp = match req.cmd.as_str() {
        "status" => ipc::Response::ok(build_status(state)),
        "reload" => ipc::Response::ok(BTreeMap::new()), // already done above
        "stop" => {
            state.shutdown.notify_one();
            ipc::Response::ok(BTreeMap::new())
        }
        _ => ipc::Response::err(),
    };

    if let Ok(mut line) = serde_json::to_string(&resp) {
        line.push('\n');
        let _ =
            tokio::time::timeout(Duration::from_secs(2), stream.write_all(line.as_bytes())).await;
    }
}

/// Splits a `get` line into `(ver, cwd, badges, env)`. The optional env block follows the
/// badges, introduced by a `client::ENV_MARK` field and an optional `client::ENV_TAG_PREFIX`
/// field; a request without it (an older client) has no env, meaning the daemon's own. Values
/// are never logged.
fn parse_get(text: &str) -> (&str, &str, Vec<String>, Option<ClientEnv<'_>>) {
    let mut fields = text.split('\u{1f}');
    let _cmd = fields.next(); // "get"
    let ver = fields.next().unwrap_or_default();
    let cwd = fields.next().unwrap_or_default();
    let mut badges = Vec::new();
    let mut marked = false;
    for f in fields.by_ref() {
        if f == crate::client::ENV_MARK {
            marked = true;
            break;
        }
        badges.push(f.to_string());
    }
    let env = marked.then(|| {
        let mut fields = fields.peekable();
        let tag = fields
            .next_if(|f| f.starts_with(crate::client::ENV_TAG_PREFIX))
            .map(|f| &f[crate::client::ENV_TAG_PREFIX.len()..]);
        let vars = fields.filter_map(|f| f.split_once('=')).collect();
        ClientEnv { vars, tag }
    });
    (ver, cwd, badges, env)
}

/// Plain-text `get` (see `client.rs`'s wire protocol doc): one line, fields separated by
/// 0x1F: `get<0x1F><ver><0x1F><cwd>(<0x1F><badge>)*(<0x1F><ENV_MARK>(<0x1F>NAME=value)*)?`.
/// Response is one line per requested badge, in order, then EOF (the connection closes when
/// this function returns).
async fn handle_text_get(state: &Arc<Daemon>, stream: &mut UnixStream, line: &[u8], now: Instant) {
    let Ok(text) = std::str::from_utf8(line) else {
        return;
    };
    let (ver, cwd, badges, env) = parse_get(text);

    schedule_reload_check(state, now);
    let cwd_opt = (!cwd.is_empty()).then_some(cwd);
    let answer = handle_get(state, cwd_opt, env.as_ref(), &badges).await;
    let out = frame_lines(response_lines(&answer.cfg, &badges, &answer.values));
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.write_all(out.as_bytes())).await;

    // Upgrade safety (design.md §1): a `get` from a different client version gets a normal
    // answer first; then, if our binary was replaced or the client is newer (see
    // `Upgrade::decide`), this daemon shuts itself down (killpg in-flight, unlink socket,
    // exit) via the same graceful path as `stop`. Zero extra client latency: no
    // synchronous notification round-trip on the hot path.
    let ours = env!("CARGO_PKG_VERSION");
    if !ver.is_empty() && ver != ours {
        state.upgrade.note_mismatch(ver);
        let exe = state
            .providers
            .blocking
            .run(
                tokio::time::Instant::now() + MAINTENANCE_BUDGET,
                state.upgrade.exe_check_job(),
            )
            .await
            .unwrap_or(Exe::Same);
        if state.upgrade.decide(ours, ver, exe, Instant::now()) == Verdict::Restart {
            state.shutdown.notify_one();
        }
    }
}

/// One `\n`-terminated line per value; embedded `\n`/`\r` would break line framing, so
/// they're replaced with a space.
fn frame_lines(lines: Vec<String>) -> String {
    let mut out = String::new();
    for value in lines {
        out.extend(
            value
                .chars()
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c }),
        );
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::cache;
    use crate::daemon::tests::test_daemon;

    #[test]
    fn frame_lines_flattens_newlines() {
        let lines = vec!["a\nb".into(), String::new(), "c\r\nd".into()];
        assert_eq!(frame_lines(lines), "a b\n\nc  d\n");
    }

    #[test]
    fn parse_get_reads_old_and_env_formats() {
        let (ver, cwd, badges, env) = parse_get("get\u{1f}1.0\u{1f}/p\u{1f}a\u{1f}b");
        assert_eq!((ver, cwd), ("1.0", "/p"));
        assert_eq!(badges, ["a", "b"]);
        assert_eq!(env, None);

        let (_, _, badges, env) = parse_get(
            "get\u{1f}1.0\u{1f}/p\u{1f}a\u{1f}\u{1e}env\u{1f}PATH=/x:/y\u{1f}K=v=w\u{1f}junk",
        );
        assert_eq!(badges, ["a"]);
        let vars = vec![("PATH", "/x:/y"), ("K", "v=w")];
        assert_eq!(env, Some(ClientEnv { vars, tag: None }));
        let (_, _, _, env) = parse_get("get\u{1f}1.0\u{1f}/p\u{1f}a\u{1f}\u{1e}env");
        let vars = vec![];
        assert_eq!(env, Some(ClientEnv { vars, tag: None }));
        let (_, _, _, env) =
            parse_get("get\u{1f}1.0\u{1f}/p\u{1f}\u{1e}env\u{1f}\u{1e}tag:ab\u{1f}P=1");
        let vars = vec![("P", "1")];
        assert_eq!(
            env,
            Some(ClientEnv {
                vars,
                tag: Some("ab")
            })
        );
    }

    async fn shutdown_requested(s: &Daemon) -> bool {
        tokio::time::timeout(Duration::from_millis(50), s.shutdown.notified())
            .await
            .is_ok()
    }

    /// Runs `handle_connection` on one end of a socketpair with `request`, returns the reply.
    async fn exchange(state: &Arc<Daemon>, request: &[u8]) -> String {
        use tokio::io::AsyncReadExt as _;
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(request).await.unwrap();
        let task = tokio::spawn(handle_connection(Arc::clone(state), server));
        let mut out = String::new();
        tokio::time::timeout(Duration::from_secs(3), client.read_to_string(&mut out))
            .await
            .unwrap()
            .unwrap();
        task.await.unwrap();
        out
    }

    #[tokio::test]
    async fn connection_dispatches_on_first_byte() {
        let state = test_daemon(cache::Power::Ac);

        // Empty line (EOF without newline) and unknown first byte: dropped, no reply.
        assert_eq!(exchange(&state, b"").await, "");
        assert_eq!(exchange(&state, b"x\n").await, "");
        assert!(!shutdown_requested(&state).await);
        // `{`: JSON admin. Unknown cmd -> error response; stop -> notifies shutdown.
        let unknown = exchange(&state, b"{\"v\":1,\"cmd\":\"nope\"}\n").await;
        assert!(unknown.contains("\"ok\":false"), "{unknown}");
        assert!(
            exchange(&state, b"{\"v\":1,\"cmd\":\"reload\"}\n")
                .await
                .contains("\"ok\":true")
        );
        assert!(!shutdown_requested(&state).await);
        assert!(
            exchange(&state, b"{\"v\":1,\"cmd\":\"stop\"}\n")
                .await
                .contains("\"ok\":true")
        );
        assert!(shutdown_requested(&state).await);
        // `g`: text get; unknown badge -> one empty line.
        let ver = env!("CARGO_PKG_VERSION");
        let req = format!("get\u{1f}{ver}\u{1f}/\u{1f}nope\n");
        assert_eq!(exchange(&state, req.as_bytes()).await, "\n");
        assert!(!shutdown_requested(&state).await);
    }

    #[tokio::test]
    async fn older_client_is_answered_and_reported_without_a_restart() {
        let state = test_daemon(cache::Power::Ac);
        let reply = exchange(&state, b"get\x1f0.0.0-old\x1f/\x1fnope\n").await;
        assert_eq!(reply, "\n");
        assert!(!shutdown_requested(&state).await);
        let status = &build_status(&state)["status"];
        assert!(
            status.contains("version mismatch: client 0.0.0-old"),
            "{status}"
        );
    }

    #[tokio::test]
    async fn newer_client_waits_for_min_uptime_before_a_restart() {
        // `test_daemon` started just now: below the minimum uptime.
        let state = test_daemon(cache::Power::Ac);
        let reply = exchange(&state, b"get\x1f999.0.0\x1f/\x1fnope\n").await;
        assert_eq!(reply, "\n");
        assert!(!shutdown_requested(&state).await);
    }
}
