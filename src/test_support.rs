//! Shared test helpers (compiled for tests only).
#![cfg(test)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A unique scratch directory under the system temp dir, removed on drop.
///
/// Names stay short on purpose: tests bind `AF_UNIX` sockets inside, and `sun_path` is
/// only 104 bytes on macOS, whose `$TMPDIR` already takes ~49 of them.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        // pid + counter is unique among live processes. A name left over from another run
        // (recycled pid) is skipped, never deleted: the counter advances until a fresh
        // directory is created.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "sf-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => panic!("create {}: {err}", path.display()),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
