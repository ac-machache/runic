//! Best-effort cross-process exclusive file lock for the memory files.
//!
//! Wraps a sidecar `{path}.lock` file with `fcntl(2)` `flock(LOCK_EX)`
//! on Unix. The lock is acquired in a blocking thread (Tokio doesn't
//! ship async fcntl) and released automatically when the [`FileLock`]
//! drops — the kernel releases the advisory lock when the fd closes.
//!
//! On non-Unix targets this is a no-op so the public API stays the same.
//! Single-process safety is still provided by the in-process
//! `tokio::sync::Mutex` inside `BoundedMemoryStore`.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct FileLock {
    // The file is held open so the OS-level lock stays acquired. Drop
    // closes it which releases the flock.
    _file: std::fs::File,
}

/// Acquire an exclusive lock on `{data_path}.lock` (sidecar file beside
/// the data file). Blocks until the lock is available; runs the blocking
/// call on a Tokio blocking thread so the runtime stays responsive.
///
/// Returns `Ok(None)` when locking is not supported on this platform —
/// callers proceed with in-process locking only.
pub async fn acquire(data_path: PathBuf) -> std::io::Result<Option<FileLock>> {
    tokio::task::spawn_blocking(move || acquire_blocking(&data_path))
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
}

fn lock_path_for(data_path: &Path) -> PathBuf {
    let mut s = data_path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

#[cfg(unix)]
fn acquire_blocking(data_path: &Path) -> std::io::Result<Option<FileLock>> {
    use std::os::unix::io::AsRawFd;

    let lock = lock_path_for(data_path);
    if let Some(parent) = lock.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock)?;

    // SAFETY: we hand the raw fd to libc::flock which only reads it and
    // installs an advisory lock. The fd remains owned by `file`, which
    // stays alive as long as the returned FileLock — when FileLock drops,
    // the file closes, the kernel drops the lock.
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Some(FileLock { _file: file }))
}

#[cfg(not(unix))]
fn acquire_blocking(_data_path: &Path) -> std::io::Result<Option<FileLock>> {
    // No-op on non-Unix — in-process Mutex is the only guard.
    Ok(None)
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[tokio::test]
    async fn lock_acquires_and_releases() {
        let dir = tempdir().unwrap();
        let data = dir.path().join("MEMORY.md");
        let lock = acquire(data.clone()).await.unwrap();
        assert!(lock.is_some());
        // Sidecar file exists.
        assert!(dir.path().join("MEMORY.md.lock").exists());
        // Dropping releases the lock so the next acquire is instant.
        drop(lock);
        let again = acquire(data).await.unwrap();
        assert!(again.is_some());
    }

    #[tokio::test]
    async fn second_acquire_blocks_until_first_releases() {
        let dir = tempdir().unwrap();
        let data = dir.path().join("MEMORY.md");

        let first = acquire(data.clone()).await.unwrap();
        assert!(first.is_some());

        // Spawn a second acquirer — it should block, NOT return.
        let data_b = data.clone();
        let second = tokio::spawn(async move { acquire(data_b).await });

        // Give it ~150ms to confirm it's blocked.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !second.is_finished(),
            "second acquire should block while first holds the lock"
        );

        // Release first → second resolves.
        drop(first);
        let result = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("second acquire should resolve after release")
            .unwrap()
            .unwrap();
        assert!(result.is_some());
    }
}
