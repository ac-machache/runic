//! `MemoryStorage` — the minimal object backend memory needs.
//!
//! Memory is read-WRITE with locking/drift, so it can't use the read-only
//! source traits the other resource domains use — but the same philosophy
//! applies: a tiny, purpose-built abstraction instead of the general
//! `runic-filesystem` backend. The store reads whole objects and writes them
//! conditionally against the revision it read; local files, object stores, and
//! databases can all implement that contract with their own concurrency model.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRevision(String);

impl MemoryRevision {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryObject {
    pub content: String,
    pub revision: MemoryRevision,
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryStorageError {
    #[error("storage io error: {0}")]
    Io(#[from] io::Error),
    #[error("write conflict for {key}")]
    Conflict { key: String },
}

fn revision_for(content: &str) -> MemoryRevision {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    content.hash(&mut h);
    MemoryRevision::new(format!("{:016x}", h.finish()))
}

/// A whole-object store keyed by relative path (e.g. `memory/MEMORY.md`).
#[async_trait]
pub trait MemoryStorage: Send + Sync {
    /// Read a whole object; `Ok(None)` if it doesn't exist.
    async fn read(&self, key: &str) -> Result<Option<MemoryObject>, MemoryStorageError>;

    /// Write a whole object if the current revision still matches.
    ///
    /// `expected = None` means "create only if missing"; `Some(revision)` means
    /// "replace only if the object is still at this revision".
    async fn write(
        &self,
        key: &str,
        content: &str,
        expected: Option<&MemoryRevision>,
    ) -> Result<MemoryRevision, MemoryStorageError>;
}

/// A real-directory store over `tokio::fs`, rooted at `root`. Keys join under
/// the root; parent directories are created on write.
pub struct LocalStorage {
    root: PathBuf,
    write_lock: AsyncMutex<()>,
}

impl LocalStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            write_lock: AsyncMutex::new(()),
        }
    }
}

#[async_trait]
impl MemoryStorage for LocalStorage {
    async fn read(&self, key: &str) -> Result<Option<MemoryObject>, MemoryStorageError> {
        match tokio::fs::read_to_string(self.root.join(key)).await {
            Ok(content) => {
                let revision = revision_for(&content);
                Ok(Some(MemoryObject { content, revision }))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn write(
        &self,
        key: &str,
        content: &str,
        expected: Option<&MemoryRevision>,
    ) -> Result<MemoryRevision, MemoryStorageError> {
        let _guard = self.write_lock.lock().await;
        let path = self.root.join(key);
        match tokio::fs::read_to_string(&path).await {
            Ok(current) => {
                let current_revision = revision_for(&current);
                if expected != Some(&current_revision) {
                    return Err(MemoryStorageError::Conflict {
                        key: key.to_string(),
                    });
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if expected.is_some() {
                    return Err(MemoryStorageError::Conflict {
                        key: key.to_string(),
                    });
                }
            }
            Err(e) => return Err(e.into()),
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, content).await?;
        Ok(revision_for(content))
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
    async fn read(&self, key: &str) -> Result<Option<MemoryObject>, MemoryStorageError> {
        Ok(self.files.lock().unwrap().get(key).map(|content| {
            let revision = revision_for(content);
            MemoryObject {
                content: content.clone(),
                revision,
            }
        }))
    }

    async fn write(
        &self,
        key: &str,
        content: &str,
        expected: Option<&MemoryRevision>,
    ) -> Result<MemoryRevision, MemoryStorageError> {
        let mut files = self.files.lock().unwrap();
        match files.get(key) {
            Some(current) => {
                let current_revision = revision_for(current);
                if expected != Some(&current_revision) {
                    return Err(MemoryStorageError::Conflict {
                        key: key.to_string(),
                    });
                }
            }
            None => {
                if expected.is_some() {
                    return Err(MemoryStorageError::Conflict {
                        key: key.to_string(),
                    });
                }
            }
        }
        files.insert(key.to_string(), content.to_string());
        Ok(revision_for(content))
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
        s.write("memory/MEMORY.md", "one", None).await.unwrap();
        let one = s.read("memory/MEMORY.md").await.unwrap().unwrap();
        assert_eq!(one.content, "one");
        assert_eq!(
            s.read("memory/MEMORY.md")
                .await
                .unwrap()
                .map(|o| o.content)
                .as_deref(),
            Some("one")
        );
        s.write("memory/MEMORY.md", "two", Some(&one.revision))
            .await
            .unwrap(); // overwrite
        assert_eq!(
            s.read("memory/MEMORY.md")
                .await
                .unwrap()
                .map(|o| o.content)
                .as_deref(),
            Some("two")
        );
    }

    #[tokio::test]
    async fn mem_roundtrip() {
        let s = MemStorage::new();
        assert_eq!(s.read("k").await.unwrap(), None);
        s.write("k", "v", None).await.unwrap();
        assert_eq!(
            s.read("k").await.unwrap().map(|o| o.content).as_deref(),
            Some("v")
        );
    }

    #[tokio::test]
    async fn stale_revision_conflicts() {
        let s = MemStorage::new();
        let first = s.write("k", "v1", None).await.unwrap();
        s.write("k", "v2", Some(&first)).await.unwrap();
        assert!(matches!(
            s.write("k", "v3", Some(&first)).await.unwrap_err(),
            MemoryStorageError::Conflict { .. }
        ));
    }
}
