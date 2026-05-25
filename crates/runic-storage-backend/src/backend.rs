//! The `StorageBackend` trait.

use async_trait::async_trait;

use crate::error::StorageError;
use crate::types::{Entry, Metadata};

/// The single abstraction every storage layer in runic depends on.
///
/// Keys are plain UTF-8 strings — no `Path`, no OS-specific separators. Each
/// backend interprets them as it sees fit (filesystem path under a base dir,
/// S3 object key, HashMap key, etc.).
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Read raw bytes for `key`.
    /// Returns `NotFound` if the key doesn't exist.
    async fn read(&self, key: &str) -> Result<Vec<u8>, StorageError>;

    /// Write `content` to `key`, replacing any existing value. Creates any
    /// necessary parent containers (directories for LocalFs, intermediate
    /// keys for namespaced backends, etc.).
    async fn write(&self, key: &str, content: &[u8]) -> Result<(), StorageError>;

    /// Delete `key`. Returns `NotFound` if it wasn't there.
    async fn delete(&self, key: &str) -> Result<(), StorageError>;

    /// Cheap "is it there?" check.
    async fn exists(&self, key: &str) -> Result<bool, StorageError>;

    /// List entries whose key starts with `prefix`. Flat — entries one level
    /// below the prefix, not recursive subtree walk. Returns entries in
    /// implementation-defined but deterministic order (BTreeMap-sorted for
    /// MemoryBackend, filesystem-order for LocalFs).
    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, StorageError>;

    /// Metadata for a single key. Returns `NotFound` if missing.
    async fn metadata(&self, key: &str) -> Result<Metadata, StorageError>;

    /// Convenience: read the value and decode as UTF-8. Default impl in terms
    /// of `read`; backends generally don't need to override.
    async fn read_to_string(&self, key: &str) -> Result<String, StorageError> {
        let bytes = self.read(key).await?;
        String::from_utf8(bytes).map_err(|err| StorageError::Decode(err.to_string()))
    }

    /// Append `content` to `key`, creating it (with just `content`) if it
    /// doesn't exist yet. Used by append-only consumers like the session
    /// event log.
    ///
    /// Default impl is read-modify-write — correct but slow for large
    /// values. Backends that can do real atomic appends (LocalFs via
    /// `OpenOptions::append`, BTreeMap by extending the vec) override this.
    async fn append(&self, key: &str, content: &[u8]) -> Result<(), StorageError> {
        let mut combined = match self.read(key).await {
            Ok(b) => b,
            Err(StorageError::NotFound { .. }) => Vec::new(),
            Err(err) => return Err(err),
        };
        combined.extend_from_slice(content);
        self.write(key, &combined).await
    }
}
