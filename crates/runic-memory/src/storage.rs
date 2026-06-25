//! `MemoryStorage` — the minimal read-write backend memory needs.
//!
//! Memory is read-WRITE with locking/drift, so it can't use the read-only
//! source traits the other resource domains use — but the same philosophy
//! applies: a tiny, purpose-built abstraction (two whole-file ops) instead of
//! the general `runic-filesystem` backend. Whole-file `read`/`write` is all the
//! `\n§\n`-delimited markdown store does; the cross-process lock and drift
//! checks live in the store, over these primitives.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;

/// A whole-file read-write store keyed by relative path (e.g. `memory/MEMORY.md`).
#[async_trait]
pub trait MemoryStorage: Send + Sync {
    /// Read a whole file; `Ok(None)` if it doesn't exist.
    async fn read(&self, key: &str) -> io::Result<Option<String>>;
    /// Write a whole file, creating or **overwriting** it.
    async fn write(&self, key: &str, content: &str) -> io::Result<()>;
}

/// A real-directory store over `tokio::fs`, rooted at `root`. Keys join under
/// the root; parent directories are created on write.
pub struct LocalStorage {
    root: PathBuf,
}

impl LocalStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[async_trait]
impl MemoryStorage for LocalStorage {
    async fn read(&self, key: &str) -> io::Result<Option<String>> {
        match tokio::fs::read_to_string(self.root.join(key)).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn write(&self, key: &str, content: &str) -> io::Result<()> {
        let path = self.root.join(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, content).await
    }
}

/// An in-memory store — the ephemeral counterpart used in tests (and anywhere
/// memory should not survive the process).
#[derive(Default)]
pub struct MemStorage {
    files: Mutex<BTreeMap<String, String>>,
}

impl MemStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MemoryStorage for MemStorage {
    async fn read(&self, key: &str) -> io::Result<Option<String>> {
        Ok(self.files.lock().unwrap().get(key).cloned())
    }

    async fn write(&self, key: &str, content: &str) -> io::Result<()> {
        self.files
            .lock()
            .unwrap()
            .insert(key.to_string(), content.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_roundtrip_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LocalStorage::new(tmp.path());
        assert_eq!(s.read("memory/MEMORY.md").await.unwrap(), None); // missing → None
        s.write("memory/MEMORY.md", "one").await.unwrap();
        assert_eq!(
            s.read("memory/MEMORY.md").await.unwrap().as_deref(),
            Some("one")
        );
        s.write("memory/MEMORY.md", "two").await.unwrap(); // overwrite
        assert_eq!(
            s.read("memory/MEMORY.md").await.unwrap().as_deref(),
            Some("two")
        );
    }

    #[tokio::test]
    async fn mem_roundtrip() {
        let s = MemStorage::new();
        assert_eq!(s.read("k").await.unwrap(), None);
        s.write("k", "v").await.unwrap();
        assert_eq!(s.read("k").await.unwrap().as_deref(), Some("v"));
    }
}
