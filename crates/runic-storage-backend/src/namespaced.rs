//! `NamespacedBackend` — prefix-based routing across multiple backends.
//!
//! Configure with `(prefix, backend)` mounts. Operations on a key route to
//! the backend mounted under the LONGEST matching prefix. The key is passed
//! to the inner backend with the prefix STRIPPED, so each inner backend
//! sees clean relative keys.

use async_trait::async_trait;
use std::sync::Arc;

use crate::backend::StorageBackend;
use crate::error::StorageError;
use crate::types::{Entry, Metadata};

pub struct NamespacedBackend {
    /// Mounts sorted by descending prefix length so the longest match is found first.
    mounts: Vec<(String, Arc<dyn StorageBackend>)>,
}

impl NamespacedBackend {
    pub fn new() -> Self {
        Self { mounts: Vec::new() }
    }

    /// Add a mount. Returns self for chaining.
    pub fn mount(mut self, prefix: impl Into<String>, backend: Arc<dyn StorageBackend>) -> Self {
        let prefix = prefix.into();
        self.mounts.push((prefix, backend));
        // Re-sort so longest prefixes are checked first.
        self.mounts.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        self
    }

    fn route<'a>(
        &'a self,
        key: &'a str,
    ) -> Result<(&'a str, &'a Arc<dyn StorageBackend>, &'a str), StorageError> {
        for (prefix, backend) in &self.mounts {
            if let Some(stripped) = key.strip_prefix(prefix.as_str()) {
                return Ok((prefix.as_str(), backend, stripped));
            }
        }
        Err(StorageError::not_found(key))
    }
}

impl Default for NamespacedBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for NamespacedBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let prefixes: Vec<&str> = self.mounts.iter().map(|(p, _)| p.as_str()).collect();
        f.debug_struct("NamespacedBackend")
            .field("mounts", &prefixes)
            .finish()
    }
}

#[async_trait]
impl StorageBackend for NamespacedBackend {
    async fn read(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let (_, backend, stripped) = self.route(key)?;
        backend.read(stripped).await
    }

    async fn write(&self, key: &str, content: &[u8]) -> Result<(), StorageError> {
        let (_, backend, stripped) = self.route(key)?;
        backend.write(stripped, content).await
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let (_, backend, stripped) = self.route(key)?;
        backend.delete(stripped).await
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        match self.route(key) {
            Ok((_, backend, stripped)) => backend.exists(stripped).await,
            Err(StorageError::NotFound { .. }) => Ok(false),
            Err(err) => Err(err),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, StorageError> {
        // Empty prefix: merge from all mounts, re-prefixing each entry with the mount.
        if prefix.is_empty() {
            let mut out: Vec<Entry> = Vec::new();
            for (mount_prefix, backend) in &self.mounts {
                for mut entry in backend.list("").await? {
                    entry.key = format!("{mount_prefix}{}", entry.key);
                    out.push(entry);
                }
            }
            out.sort_by(|a, b| a.key.cmp(&b.key));
            return Ok(out);
        }

        // Non-empty prefix: route to the matching mount and re-prefix entries.
        let (mount_prefix, backend, stripped) = self.route(prefix)?;
        let mount_prefix = mount_prefix.to_string();
        let inner_entries = backend.list(stripped).await?;
        Ok(inner_entries
            .into_iter()
            .map(|mut e| {
                e.key = format!("{mount_prefix}{}", e.key);
                e
            })
            .collect())
    }

    async fn metadata(&self, key: &str) -> Result<Metadata, StorageError> {
        let (_, backend, stripped) = self.route(key)?;
        backend.metadata(stripped).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryBackend;

    fn mem() -> Arc<MemoryBackend> {
        Arc::new(MemoryBackend::new())
    }

    #[tokio::test]
    async fn unknown_prefix_returns_not_found() {
        let ns = NamespacedBackend::new().mount("local/", mem());
        let err = ns.read("unmounted/key").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound { .. }));
    }

    #[tokio::test]
    async fn routes_to_correct_backend_by_prefix() {
        let a = mem();
        let b = mem();
        let ns = NamespacedBackend::new()
            .mount("a/", a.clone())
            .mount("b/", b.clone());

        ns.write("a/x", b"in-a").await.unwrap();
        ns.write("b/x", b"in-b").await.unwrap();

        assert_eq!(a.read("x").await.unwrap(), b"in-a");
        assert_eq!(b.read("x").await.unwrap(), b"in-b");
        assert_eq!(ns.read("a/x").await.unwrap(), b"in-a");
        assert_eq!(ns.read("b/x").await.unwrap(), b"in-b");
    }

    #[tokio::test]
    async fn longest_prefix_wins() {
        let short = mem();
        let long = mem();
        let ns = NamespacedBackend::new()
            .mount("foo/", short.clone())
            .mount("foo/bar/", long.clone());

        ns.write("foo/bar/baz", b"long").await.unwrap();
        // Long backend should receive the stripped key "baz".
        assert_eq!(long.read("baz").await.unwrap(), b"long");
        assert!(short.list("").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_with_mount_prefix_routes_and_reprefixes() {
        let a = mem();
        let ns = NamespacedBackend::new().mount("docs/", a.clone());
        a.write("intro.md", b"").await.unwrap();
        a.write("guide.md", b"").await.unwrap();

        let entries = ns.list("docs/").await.unwrap();
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["docs/guide.md", "docs/intro.md"]);
    }

    #[tokio::test]
    async fn list_empty_prefix_merges_all_mounts() {
        let a = mem();
        let b = mem();
        a.write("x", b"").await.unwrap();
        b.write("y", b"").await.unwrap();
        let ns = NamespacedBackend::new()
            .mount("a/", a)
            .mount("b/", b);

        let entries = ns.list("").await.unwrap();
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a/x", "b/y"]);
    }

    #[tokio::test]
    async fn exists_routes_correctly_and_returns_false_for_unknown_prefix() {
        let a = mem();
        a.write("k", b"").await.unwrap();
        let ns = NamespacedBackend::new().mount("a/", a);

        assert!(ns.exists("a/k").await.unwrap());
        assert!(!ns.exists("a/missing").await.unwrap());
        assert!(!ns.exists("b/anything").await.unwrap());
    }
}
