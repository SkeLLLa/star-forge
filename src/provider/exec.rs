//! Bounded subprocess and HTTP execution.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncReadExt as _;
use tokio::process::{Child, ChildStdout, Command};
use tokio::time::Instant;

use super::{ProviderError, Shared};

/// Extra env vars and the shared in-flight registry, bundled to keep `run_with_timeout`'s
/// argument count sane.
pub(super) struct RunOpts<'a> {
    pub(super) envs: &'a [(&'a str, &'a str)],
    /// Inherited variables to remove (applied before `envs`).
    pub(super) unset: &'a [String],
    pub(super) shared: &'a Shared,
}

/// Own the group together with the child, including while a blocking spawn's result
/// is waiting for delivery. Cancellation or a late result must kill descendants too.
struct SpawnedChild {
    child: Child,
    pgid: i32,
    inflight: Arc<Mutex<HashSet<i32>>>,
    armed: bool,
}

impl SpawnedChild {
    fn kill_group(&self) {
        if self.armed {
            // SAFETY: `killpg` takes plain integers and touches no memory; `armed` means the
            // group is still registered (not yet reaped), so the pgid can't have been reused.
            unsafe { libc::killpg(self.pgid, libc::SIGKILL) };
        }
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let result = self.child.wait().await;
        if result.is_ok() {
            self.armed = false;
            self.inflight
                .lock()
                .expect("inflight lock poisoned")
                .remove(&self.pgid);
        }
        result
    }
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        self.kill_group();
        // No panic in Drop: it may run during unwinding.
        self.inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.pgid);
        // Child's kill_on_drop reaps the leader through tokio's orphan reaper.
    }
}

/// Consumes a child's stdout as it arrives. An error stops the read and kills the group.
pub(super) trait Sink {
    type Output;
    fn feed(&mut self, chunk: &[u8]) -> Result<(), ProviderError>;
    fn finish(self) -> Self::Output;
}

/// Buffers the whole output, failing with `TooLarge` past `cap` bytes.
struct Capped {
    buf: Vec<u8>,
    cap: usize,
}

impl Sink for Capped {
    type Output = Vec<u8>;

    fn feed(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > self.cap {
            return Err(ProviderError::TooLarge);
        }
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Runs `prog args` (stdin from `/dev/null`), killing the whole process group on timeout
/// or oversized stdout. The capped stdout read and `wait()` both happen inside `deadline`; on
/// the success path the leader is reaped and never killed (its pgid may be reused once dead).
pub(super) async fn run_with_timeout(
    prog: &str,
    args: &[String],
    cwd: Option<&Path>,
    deadline: Instant,
    cap: usize,
    opts: RunOpts<'_>,
) -> Result<Vec<u8>, ProviderError> {
    let sink = Capped {
        buf: Vec::new(),
        cap,
    };
    run_into(prog, args, cwd, deadline, sink, opts).await
}

/// [`run_with_timeout`], streaming stdout into `sink` instead of a capped buffer.
pub(super) async fn run_into<S: Sink>(
    prog: &str,
    args: &[String],
    cwd: Option<&Path>,
    deadline: Instant,
    mut sink: S,
    opts: RunOpts<'_>,
) -> Result<S::Output, ProviderError> {
    if opts.shared.stopping.load(Ordering::SeqCst) {
        return Err(ProviderError::Stopped);
    }
    let mut cmd = Command::new(prog);
    let prog = prog.to_owned();
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for k in opts.unset {
        cmd.env_remove(k);
    }
    for (k, v) in opts.envs {
        cmd.env(k, v);
    }

    let shared = opts.shared.clone();
    let mut child = opts
        .shared
        .pool_for(cwd)
        .run(deadline, move || {
            if shared.stopping.load(Ordering::SeqCst) {
                return Err(ProviderError::Stopped);
            }
            // Command::spawn waits for exec/chdir and can itself hang on a mount.
            let child = cmd
                .spawn()
                .map_err(|e| ProviderError::Spawn(format!("{prog}: {e}")))?;
            let pgid = child
                .id()
                .ok_or_else(|| ProviderError::Spawn("child has no pid".into()))?
                .cast_signed();
            let spawned = SpawnedChild {
                child,
                pgid,
                inflight: Arc::clone(&shared.inflight),
                armed: true,
            };
            {
                let mut registry = shared.inflight.lock().expect("inflight lock poisoned");
                if shared.stopping.load(Ordering::SeqCst) {
                    // Drop outside the registry lock: the guard also removes its pid.
                    drop(registry);
                    return Err(ProviderError::Stopped);
                }
                registry.insert(pgid);
            }
            Ok(spawned)
        })
        .await??;

    let mut stdout = child.child.stdout.take().expect("stdout is piped");

    let work = async {
        match read_into(&mut stdout, &mut sink).await {
            Ok(()) => match child.wait().await {
                Ok(status) if status.success() => Ok(sink.finish()),
                Ok(status) => Err(ProviderError::NonZeroExit(status.code().unwrap_or(-1))),
                Err(e) => Err(e.into()),
            },
            Err(e) => {
                // Read stopped early (e.g. TooLarge): the child may be blocked writing to
                // a full pipe and would never exit on its own, so kill before reaping.
                child.kill_group();
                let _ = child.wait().await;
                Err(e)
            }
        }
    };

    match tokio::time::timeout_at(deadline, work).await {
        Ok(r) => r,
        Err(_elapsed) => {
            // The leader is not yet reaped here (zombie at worst), so its pgid cannot
            // have been recycled.
            child.kill_group();
            let _ = child.wait().await;
            Err(ProviderError::Timeout)
        }
    }
}

async fn read_into<S: Sink>(stdout: &mut ChildStdout, sink: &mut S) -> Result<(), ProviderError> {
    let mut chunk = [0u8; 8192];
    loop {
        let n = stdout.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        sink.feed(&chunk[..n])?;
    }
}

pub(super) async fn http_fetch(
    client: &reqwest::Client,
    url: &str,
    headers: &BTreeMap<String, String>,
    deadline: Instant,
    cap: usize,
) -> Result<Vec<u8>, ProviderError> {
    let mut builder = client.get(url);
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    let fut = async {
        let mut resp = builder
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ProviderError::HttpStatus(resp.status().as_u16()));
        }
        let mut buf = Vec::new();
        while let Some(part) = resp
            .chunk()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?
        {
            buf.extend_from_slice(&part);
            if buf.len() > cap {
                return Err(ProviderError::TooLarge);
            }
        }
        Ok(buf)
    };
    tokio::time::timeout_at(deadline, fut)
        .await
        .unwrap_or(Err(ProviderError::Timeout))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    #[tokio::test]
    async fn run_with_timeout_kills_hanging_command() {
        let shared = Shared::new();
        let deadline = Instant::now() + Duration::from_millis(100);
        let args = ["0.5".to_string()];
        let result = run_with_timeout(
            "sleep",
            &args,
            None,
            deadline,
            1024,
            RunOpts {
                envs: &[],
                unset: &[],
                shared: &shared,
            },
        )
        .await;
        assert!(matches!(result, Err(ProviderError::Timeout)));
        assert!(shared.inflight.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_with_timeout_caps_stdout() {
        let shared = Shared::new();
        let deadline = Instant::now() + Duration::from_millis(500);
        let args = ["-c".to_string(), "yes | head -c 100000".to_string()];
        let result = run_with_timeout(
            "sh",
            &args,
            None,
            deadline,
            1024,
            RunOpts {
                envs: &[],
                unset: &[],
                shared: &shared,
            },
        )
        .await;
        assert!(matches!(result, Err(ProviderError::TooLarge)));
    }

    /// Count of live processes whose argv is exactly `sleep <marker>` (`ps` works on Linux
    /// and macOS and is required: a missing `ps` fails the test; zombies print as
    /// `[sleep] <defunct>`/`(sleep)`, so they never count).
    fn proc_cmdline_count(marker: &str) -> usize {
        let out = std::process::Command::new("ps")
            .args(["-A", "-ww", "-o", "args="])
            .output()
            .expect("this test needs `ps`");
        let want = format!("sleep {marker}");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| line.trim() == want)
            .count()
    }

    #[tokio::test]
    async fn cancelling_a_process_future_releases_its_group_registration() {
        let shared = Shared::new();
        let owned = shared.clone();
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let marker = format!("{}.{}", 9_300_000 + std::process::id(), nanos % 1000);
        let running = tokio::spawn({
            let marker = marker.clone();
            async move {
                run_with_timeout(
                    "sh",
                    &["-c".into(), format!("sleep {marker} | sleep {marker}")],
                    None,
                    Instant::now() + Duration::from_secs(20),
                    1024,
                    RunOpts {
                        envs: &[],
                        unset: &[],
                        shared: &owned,
                    },
                )
                .await
            }
        });
        let started = tokio::time::timeout(Duration::from_secs(2), async {
            while shared.inflight.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let both_stages_started = tokio::time::timeout(Duration::from_secs(2), async {
            while proc_cmdline_count(&marker) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        // Always abort the task, including a failed start, to avoid leaving the test's
        // process alive if an assertion fails.
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        assert!(started.is_ok(), "process never registered");
        assert!(
            both_stages_started.is_ok(),
            "pipeline never started both stages"
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !shared.inflight.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(shared.inflight.lock().unwrap().is_empty());
        tokio::time::timeout(Duration::from_secs(2), async {
            while proc_cmdline_count(&marker) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
