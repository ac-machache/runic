//! `RootedBackend` — chroot-style wrapper that prepends a fixed prefix
//! to every key before delegating to an inner [`StorageBackend`].
//!
//! Where [`crate::NamespacedBackend`] strips a matched prefix on the way
//! in (mount-and-route semantics), `RootedBackend` does the reverse —
//! the caller addresses the backend with clean relative keys (`"intro.md"`)
//! and the wrapper rewrites them to the equivalent absolute keys
//! (`"wikis/intro.md"`) for the inner backend. List results have the
//! prefix stripped back out so the caller sees the same clean view.
//!
//! Used by sub-agents that get an isolated view of a sub-tree of the
//! parent's storage — they spawn with shell tools bound to a
//! `RootedBackend(parent_storage, "wikis")` and see only that
//! sub-tree, exactly like a Unix chroot.

use async_trait::async_trait;
use std::sync::Arc;

use crate::backend::StorageBackend;
use crate::error::StorageError;
use crate::types::{Entry, Metadata};

pub struct RootedBackend {
    inner: Arc<dyn StorageBackend>,
    /// Prefix applied to every incoming key. Stored with a trailing `/`
    /// so we never have to special-case the join. Empty when the wrapper
    /// is configured as a pass-through (rare; mostly a test convenience).
    root: String,
}

impl std::fmt::Debug for RootedBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootedBackend")
            .field("root", &self.root)
            .finish()
    }
}

impl RootedBackend {
    pub fn new(inner: Arc<dyn StorageBackend>, root: impl Into<String>) -> Self {
        let raw = root.into();
        let mut root = raw.trim_matches('/').to_string();
        if !root.is_empty() {
            root.push('/');
        }
        Self { inner, root }
    }

    fn join(&self, key: &str) -> String {
        let stripped = key.trim_start_matches('/');
        if stripped.is_empty() {
            // Caller asked for "root itself" — return our prefix sans trailing `/`
            // so the inner backend lists/reads its mount point cleanly.
            self.root.trim_end_matches('/').to_string()
        } else {
            format!("{}{}", self.root, stripped)
        }
    }

    fn strip(&self, key: &str) -> String {
        if self.root.is_empty() {
            return key.to_string();
        }
        let trimmed = self.root.trim_end_matches('/');
        key.strip_prefix(&self.root)
            .or_else(|| key.strip_prefix(trimmed))
            .unwrap_or(key)
            .to_string()
    }
}

#[async_trait]
impl StorageBackend for RootedBackend {
    async fn read(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.read(&self.join(key)).await
    }

    async fn write(&self, key: &str, content: &[u8]) -> Result<(), StorageError> {
        self.inner.write(&self.join(key), content).await
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.inner.delete(&self.join(key)).await
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        self.inner.exists(&self.join(key)).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, StorageError> {
        let inner_entries = self.inner.list(&self.join(prefix)).await?;
        Ok(inner_entries
            .into_iter()
            .map(|mut e| {
                e.key = self.strip(&e.key);
                e
            })
            .collect())
    }

    async fn metadata(&self, key: &str) -> Result<Metadata, StorageError> {
        self.inner.metadata(&self.join(key)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryBackend;

    fn inner() -> Arc<dyn StorageBackend> {
        Arc::new(MemoryBackend::new())
    }

    #[tokio::test]
    async fn writes_land_under_the_root_in_the_inner_backend() {
        let backend = inner();
        let rooted = RootedBackend::new(backend.clone(), "wikis");
        rooted.write("intro.md", b"hello").await.unwrap();
        // The inner backend stores under the rooted prefix.
        let raw = backend.read("wikis/intro.md").await.unwrap();
        assert_eq!(raw, b"hello");
    }

    #[tokio::test]
    async fn reads_see_only_the_rooted_subtree() {
        let backend = inner();
        backend
            .write("wikis/inside.md", b"in")
            .await
            .unwrap();
        backend.write("not-wikis/outside.md", b"out").await.unwrap();
        let rooted = RootedBackend::new(backend, "wikis");
        assert_eq!(rooted.read("inside.md").await.unwrap(), b"in");
        let err = rooted.read("outside.md").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound { .. }));
    }

    #[tokio::test]
    async fn list_strips_the_prefix_from_results() {
        let backend = inner();
        backend.write("wikis/a.md", b"").await.unwrap();
        backend.write("wikis/b.md", b"").await.unwrap();
        backend.write("other.md", b"").await.unwrap();
        let rooted = RootedBackend::new(backend, "wikis");
        let mut keys: Vec<String> = rooted
            .list("")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect();
        keys.sort();
        assert_eq!(keys, vec!["a.md", "b.md"]);
    }

    #[tokio::test]
    async fn list_with_subprefix_routes_inside_the_root() {
        let backend = inner();
        backend.write("wikis/sub/x.md", b"").await.unwrap();
        backend.write("wikis/sub/y.md", b"").await.unwrap();
        let rooted = RootedBackend::new(backend, "wikis");
        let mut keys: Vec<String> = rooted
            .list("sub")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect();
        keys.sort();
        assert_eq!(keys, vec!["sub/x.md", "sub/y.md"]);
    }

    #[tokio::test]
    async fn root_normalises_leading_and_trailing_slashes() {
        let backend = inner();
        let rooted = RootedBackend::new(backend.clone(), "/wikis/");
        rooted.write("intro.md", b"x").await.unwrap();
        assert_eq!(backend.read("wikis/intro.md").await.unwrap(), b"x");
    }

    #[tokio::test]
    async fn empty_root_is_pure_passthrough() {
        let backend = inner();
        let rooted = RootedBackend::new(backend.clone(), "");
        rooted.write("a.md", b"x").await.unwrap();
        assert_eq!(backend.read("a.md").await.unwrap(), b"x");
    }

    #[tokio::test]
    async fn exists_round_trips_under_root() {
        let backend = inner();
        backend.write("wikis/found.md", b"").await.unwrap();
        let rooted = RootedBackend::new(backend, "wikis");
        assert!(rooted.exists("found.md").await.unwrap());
        assert!(!rooted.exists("missing.md").await.unwrap());
    }
}
