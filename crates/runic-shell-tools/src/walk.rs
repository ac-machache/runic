//! Recursive walker over a [`StorageBackend`].
//!
//! `StorageBackend::list` returns one directory level. `walk_files` does
//! the iterative BFS so [`crate::GlobTool`] and [`crate::GrepTool`] can
//! consider every file under a prefix without hand-rolling the loop in
//! two places.
//!
//! Why iterative (queue) and not recursive (calls): tokio futures don't
//! play nicely with async recursion without boxing every step. A queue
//! keeps everything `async fn` shaped and the boxing cost out of the
//! hot loop.

use std::sync::Arc;

use runic_storage_backend::{EntryKind, StorageBackend};

use crate::error::ShellToolError;

/// Walk the tree starting at `root` and return every File entry's key.
/// Directories are descended into; files are collected. Total file
/// count is capped at `max_files` so a runaway scan can't OOM the agent.
pub async fn walk_files(
    storage: &Arc<dyn StorageBackend>,
    root: &str,
    max_files: usize,
) -> Result<Vec<String>, ShellToolError> {
    let mut found: Vec<String> = Vec::new();
    let mut queue: Vec<String> = vec![root.to_string()];

    while let Some(prefix) = queue.pop() {
        let entries = storage
            .list(&prefix)
            .await
            .map_err(|e| ShellToolError::Storage(e.to_string()))?;
        for entry in entries {
            match entry.kind {
                EntryKind::File => {
                    found.push(entry.key);
                    if found.len() >= max_files {
                        return Ok(found);
                    }
                }
                EntryKind::Directory => queue.push(entry.key),
            }
        }
    }

    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_storage_backend::MemoryBackend;

    async fn seeded() -> Arc<dyn StorageBackend> {
        let backend: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        backend.write("a.md", b"alpha").await.unwrap();
        backend.write("dir/b.md", b"beta").await.unwrap();
        backend.write("dir/sub/c.md", b"gamma").await.unwrap();
        backend.write("dir/sub/d.txt", b"delta").await.unwrap();
        backend
    }

    #[tokio::test]
    async fn finds_every_file_from_root() {
        let storage = seeded().await;
        let mut keys = walk_files(&storage, "", usize::MAX).await.unwrap();
        keys.sort();
        assert_eq!(
            keys,
            vec!["a.md", "dir/b.md", "dir/sub/c.md", "dir/sub/d.txt"]
        );
    }

    #[tokio::test]
    async fn scopes_to_subprefix() {
        let storage = seeded().await;
        let mut keys = walk_files(&storage, "dir/sub", usize::MAX).await.unwrap();
        keys.sort();
        assert_eq!(keys, vec!["dir/sub/c.md", "dir/sub/d.txt"]);
    }

    #[tokio::test]
    async fn cap_short_circuits_walk() {
        let storage = seeded().await;
        let keys = walk_files(&storage, "", 2).await.unwrap();
        assert_eq!(keys.len(), 2);
    }
}
