//! File identity stamps shared by config reload, `watch` fingerprints and the git fingerprint.

use std::path::Path;

/// Identity of a file (following symlinks): changes on rewrite, rename-on-save and retarget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId {
    dev: u64,
    ino: u64,
    len: u64,
    ctime: i64,
    ctime_nsec: i64,
}

/// Not mtime alone: `cp -p`/`rsync -a` can preserve it, and Nix store files all share mtime 1,
/// so re-pointing a symlink (home-manager) would go unnoticed. ctime can't be set from
/// userspace, and the inode changes on rename-on-save and on a symlink retarget.
pub fn file_id(p: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok()?;
    Some(FileId {
        dev: m.dev(),
        ino: m.ino(),
        len: m.len(),
        ctime: m.ctime(),
        ctime_nsec: m.ctime_nsec(),
    })
}
