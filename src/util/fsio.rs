//! Durable file I/O primitives: atomic rename-publish writes, flock-based
//! exclusion, and order-stable globbing.

use std::path::{Path, PathBuf};

/// Take a `flock(2)` on an open file. `block` = wait for the holder (LOCK_EX);
/// otherwise fail fast (LOCK_EX|LOCK_NB). `true` = we hold it now. flock is
/// kernel-managed per-inode state: it dies with the process, so there is never
/// a stale lock to reclaim and no PID-liveness guessing. This is the ONE
/// extern-"C" flock declaration — `store.rs` (per-directory writer lock) and
/// `run.rs` (single-instance pulse lock) both route through it.
#[cfg(unix)]
pub(crate) fn flock_file(f: &std::fs::File, block: bool) -> bool {
    use std::os::unix::io::AsRawFd;
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    unsafe extern "C" {
        fn flock(fd: i32, op: i32) -> i32;
    }
    let op = if block { LOCK_EX } else { LOCK_EX | LOCK_NB };
    unsafe { flock(f.as_raw_fd(), op) == 0 }
}
#[cfg(not(unix))]
pub(crate) fn flock_file(_f: &std::fs::File, _block: bool) -> bool {
    true // best-effort: flock-based exclusion is unix-only
}

/// A process-wide monotonic nonce for temp-file names. `now_unix()` alone is
/// second-precision, so two atomic writes to the SAME target within one second
/// (easy under test or a busy mailbox) could collide on the temp name; the
/// counter makes every temp name unique within the process, and the pid keeps
/// processes apart.
pub fn temp_nonce() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// fsync the DIRECTORY containing `path`, so the rename that just landed in it
/// is durable (a crash after rename can otherwise lose the directory entry).
/// Unix-only (opening a directory read-only works there); a failure is ignored
/// by callers that treat durability as best-effort.
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}
#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Atomically write `contents` to `path`: write a sibling temp file, fsync, then
/// `rename` over the target. `rename(2)` on the same filesystem is atomic, so a
/// concurrent reader (the pulse re-sensing each beat) never sees a half-written
/// goal/PLAYBOOK/sensor — it sees either the old bytes or the new, never a torn
/// truncation. This is what lets the contract's STEER verbs promise atomic
/// writes that a raw `fs::write` (truncate-then-write) cannot. After the rename
/// the parent directory is fsync'd too, so the new entry survives a crash.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, contents, None)
}

/// [`write_atomic`] with an optional unix permission mode applied to the TEMP
/// file BEFORE the rename, so the target is never observable with the wrong
/// mode (e.g. a sensor script must never be visible non-executable).
pub fn write_atomic_mode(
    path: &Path,
    contents: &[u8],
    #[cfg_attr(not(unix), allow(unused_variables))] mode: Option<u32>,
) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    // Unique temp name in the SAME dir (so rename stays on one filesystem).
    // pid + second + process-wide counter: unique across processes AND within
    // the same second in one process.
    let pid = std::process::id();
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("tmp");
    let tmp = dir.join(format!(
        ".{stem}.{pid}.{}.{}.tmp",
        super::now_unix(),
        temp_nonce()
    ));
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        // Mode BEFORE sync_all: the fsync then covers the permission metadata
        // too, so a crash right after the rename can't resurrect the file
        // without its mode (e.g. a sensor script losing its exec bit).
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        // Durability of the rename itself: fsync the parent dir so the entry
        // survives a crash. Best-effort — the data bytes are already synced.
        let _ = sync_parent_dir(path);
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Sorted absolute paths of `dir/*.<ext>` (best-effort: an unreadable dir yields
/// an empty list). Sorting makes any derived hash / prompt order-stable.
pub fn sorted_glob(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == ext))
        .collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_replaces_existing_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!(
            "looop-wa-{}-{}",
            std::process::id(),
            crate::util::now_unix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("sub").join("goal.md");
        // Writes through a not-yet-existing parent dir.
        write_atomic(&target, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");
        // Overwrites in place.
        write_atomic(&target, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");
        // No leftover temp siblings.
        let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
