//! `MemoryBackend` — pure in-memory storage. Primarily for tests.
//!
//! Thread-safe via a single `Mutex<BTreeMap<...>>`. BTreeMap (not HashMap)
//! so `list` returns deterministically sorted results without an extra sort.

use async_trait::async_trait;
use chrono::Utc;
use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::backend::StorageBackend;
use crate::error::StorageError;
use crate::types::{Entry, EntryKind, Metadata};

#[derive(Debug, Clone)]
struct MemoryEntry {
    bytes: Vec<u8>,
    modified: chrono::DateTime<Utc>,
}

#[derive(Default)]
pub struct MemoryBackend {
    entries: Mutex<BTreeMap<String, MemoryEntry>>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for MemoryBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.entries.lock().map(|g| g.len()).unwrap_or(0);
        f.debug_struct("MemoryBackend").field("entries", &len).finish()
    }
}

#[async_trait]
impl StorageBackend for MemoryBackend {
    async fn read(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let entries = self.entries.lock().expect("memory backend lock poisoned");
        entries
            .get(key)
            .map(|e| e.bytes.clone())
            .ok_or_else(|| StorageError::not_found(key))
    }

    async fn write(&self, key: &str, content: &[u8]) -> Result<(), StorageError> {
        let mut entries = self.entries.lock().expect("memory backend lock poisoned");
        entries.insert(
            key.to_string(),
            MemoryEntry {
                bytes: content.to_vec(),
                modified: Utc::now(),
            },
        );
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let mut entries = self.entries.lock().expect("memory backend lock poisoned");
        entries
            .remove(key)
            .map(|_| ())
            .ok_or_else(|| StorageError::not_found(key))
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        let entries = self.entries.lock().expect("memory backend lock poisoned");
        Ok(entries.contains_key(key))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, StorageError> {
        let entries = self.entries.lock().expect("memory backend lock poisoned");
        let out: Vec<Entry> = entries
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| Entry {
                key: k.clone(),
                kind: EntryKind::File,
                size: Some(v.bytes.len() as u64),
                modified: Some(v.modified),
            })
            .collect();
        Ok(out)
    }

    async fn metadata(&self, key: &str) -> Result<Metadata, StorageError> {
        let entries = self.entries.lock().expect("memory backend lock poisoned");
        entries
            .get(key)
            .map(|v| Metadata {
                kind: EntryKind::File,
                size: v.bytes.len() as u64,
                modified: Some(v.modified),
            })
            .ok_or_else(|| StorageError::not_found(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn new_backend_is_empty() {
        let backend = MemoryBackend::new();
        let entries = backend.list("").await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn metadata_tracks_creation_time() {
        let backend = MemoryBackend::new();
        let before = Utc::now();
        backend.write("k", b"v").await.unwrap();
        let meta = backend.metadata("k").await.unwrap();
        assert!(meta.modified.unwrap() >= before);
    }

    #[tokio::test]
    async fn arc_clones_share_state() {
        let backend = Arc::new(MemoryBackend::new());
        let clone = backend.clone();
        backend.write("shared", b"yes").await.unwrap();
        let content = clone.read("shared").await.unwrap();
        assert_eq!(content, b"yes");
    }

    #[tokio::test]
    async fn list_returns_sorted_entries() {
        let backend = MemoryBackend::new();
        backend.write("z", b"").await.unwrap();
        backend.write("a", b"").await.unwrap();
        backend.write("m", b"").await.unwrap();
        let entries = backend.list("").await.unwrap();
        let keys: Vec<_> = entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "m", "z"]);
    }
}
