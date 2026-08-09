//! Cross-process advisory locks for model-writable asset directories.
//!
//! Multiple sigil instances can run at the same time (an interactive
//! REPL, an MCP server, several pipe-mode jobs) and all of them may
//! write the same harness / tools directories via the evolution tools
//! (`create_lua_tool`, `improve_harness`, `skill_rollback`). Without a
//! lock, two processes can interleave "read old content → archive →
//! write new content" and corrupt the version library or leave a
//! half-written asset.
//!
//! The lock is a `<dir>/.sigil.lock` file locked exclusively with
//! `fd-lock` (flock on Unix, LockFileEx on Windows). It is advisory:
//! every writer must go through [`with_dir_lock`]. Reads never lock.

use std::path::Path;

/// Run `f` while holding an exclusive advisory lock on `dir`.
///
/// The lock file is `<dir>/.sigil.lock` (created on demand). Use this
/// around the whole read-archive-write sequence of an evolution tool —
/// never inside a helper that may itself be called from a locked
/// context, or the same process would deadlock on its own lock.
pub fn with_dir_lock<T>(dir: &Path, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    std::fs::create_dir_all(dir)?;
    let lock_path = dir.join(".sigil.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock
        .write()
        .map_err(|e| anyhow::anyhow!("locking {}: {e}", lock_path.display()))?;
    f()
}

/// Same as [`with_dir_lock`], but non-blocking: returns `None` when
/// another process currently holds the lock. Used for tests and for
/// operations that prefer skipping over waiting.
#[cfg(test)]
pub fn try_dir_lock<T>(
    dir: &Path,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    std::fs::create_dir_all(dir)?;
    let lock_path = dir.join(".sigil.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = match lock.try_write() {
        Ok(g) => g,
        // Another process (or handle) holds the lock — skip.
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("locking {}: {e}", lock_path.display())),
    };
    f().map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_across_handles() {
        let dir = std::env::temp_dir().join(format!("sigil-lock-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // First handle takes the lock.
        let got = try_dir_lock(&dir, || {
            // Second handle (simulating another process) can't take it.
            let second = try_dir_lock(&dir, || Ok::<_, anyhow::Error>(42));
            assert!(second.unwrap().is_none());
            Ok::<_, anyhow::Error>("first")
        })
        .unwrap();
        assert_eq!(got, Some("first"));

        // After the first guard drops, a fresh lock succeeds.
        let again = try_dir_lock(&dir, || Ok::<_, anyhow::Error>("again")).unwrap();
        assert_eq!(again, Some("again"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
