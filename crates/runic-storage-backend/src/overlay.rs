//! `OverlayBackend` — first-hit-wins composition over multiple backends.
//!
//! Models the "skills come from local AND cloud" use case. Configure with
//! an ordered list of backends; reads try each in turn until one returns
//! a hit. Writes always go to the primary (index 0). Lists merge across
//! all layers, deduplicating by key with primary winning conflicts.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::backend::StorageBackend;
use crate::error::StorageError;
use crate::types::{Entry, Metadata};

pub struct OverlayBackend {
    layers: Vec<Arc<dyn StorageBackend>>,
}

impl OverlayBackend {
    /// Create an overlay with the given layers. The first element is the
    /// primary — all writes and deletes go there.
    pub fn new(layers: Vec<Arc<dyn StorageBackend>>) -> Self {
        Self { layers }
    }

    fn primary(&self) -> Result<&Arc<dyn StorageBackend>, StorageError> {
        self.layers
            .first()
            .ok_or_else(|| StorageError::Unsupported("OverlayBackend has no layers".into()))
    }
}

impl std::fmt::Debug for OverlayBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayBackend")
            .field("layers", &self.layers.len())
            .finish()
    }
}

#[async_trait]
impl StorageBackend for OverlayBackend {
    async fn read(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        if self.layers.is_empty() {
            return Err(StorageError::not_found(key));
        }
        for layer in &self.layers {
            match layer.read(key).await {
                Ok(bytes) => return Ok(bytes),
                Err(StorageError::NotFound { .. }) => continue,
                Err(err) => return Err(err),
            }
        }
        Err(StorageError::not_found(key))
    }

    async fn write(&self, key: &str, content: &[u8]) -> Result<(), StorageError> {
        self.primary()?.write(key, content).await
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.primary()?.delete(key).await
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        for layer in &self.layers {
            if layer.exists(key).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, StorageError> {
        // Merge by key; primary (first layer) wins on collision.
        let mut merged: BTreeMap<String, Entry> = BTreeMap::new();
        // Walk in reverse so primary overwrites fallback entries.
        for layer in self.layers.iter().rev() {
            for entry in layer.list(prefix).await? {
                merged.insert(entry.key.clone(), entry);
            }
        }
        Ok(merged.into_values().collect())
    }

    async fn metadata(&self, key: &str) -> Result<Metadata, StorageError> {
        if self.layers.is_empty() {
            return Err(StorageError::not_found(key));
        }
        for layer in &self.layers {
            match layer.metadata(key).await {
                Ok(meta) => return Ok(meta),
                Err(StorageError::NotFound { .. }) => continue,
                Err(err) => return Err(err),
            }
        }
        Err(StorageError::not_found(key))
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
    async fn empty_layers_treat_everything_as_not_found() {
        let overlay = OverlayBackend::new(Vec::new());
        let err = overlay.read("any").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound { .. }));
    }

    #[tokio::test]
    async fn read_falls_through_to_second_layer() {
        let primary = mem();
        let fallback = mem();
        fallback.write("only_in_fallback", b"from-fallback").await.unwrap();

        let overlay = OverlayBackend::new(vec![primary.clone(), fallback.clone()]);
        let content = overlay.read("only_in_fallback").await.unwrap();
        assert_eq!(content, b"from-fallback");
    }

    #[tokio::test]
    async fn read_prefers_primary_when_both_have_key() {
        let primary = mem();
        let fallback = mem();
        primary.write("shared", b"primary").await.unwrap();
        fallback.write("shared", b"fallback").await.unwrap();

        let overlay = OverlayBackend::new(vec![primary.clone(), fallback.clone()]);
        let content = overlay.read("shared").await.unwrap();
        assert_eq!(content, b"primary");
    }

    #[tokio::test]
    async fn write_always_lands_in_primary() {
        let primary = mem();
        let fallback = mem();
        let overlay = OverlayBackend::new(vec![primary.clone(), fallback.clone()]);

        overlay.write("new_key", b"data").await.unwrap();

        assert!(primary.exists("new_key").await.unwrap());
        assert!(!fallback.exists("new_key").await.unwrap());
    }

    #[tokio::test]
    async fn list_merges_and_dedupes() {
        let primary = mem();
        let fallback = mem();
        primary.write("a", b"primary-a").await.unwrap();
        primary.write("b", b"primary-b").await.unwrap();
        fallback.write("b", b"fallback-b").await.unwrap();
        fallback.write("c", b"fallback-c").await.unwrap();

        let overlay = OverlayBackend::new(vec![primary, fallback]);
        let entries = overlay.list("").await.unwrap();
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
        // The primary entry for "b" should win — verify by reading through overlay.
        let b_content = overlay.read("b").await.unwrap();
        assert_eq!(b_content, b"primary-b");
    }

    #[tokio::test]
    async fn delete_only_affects_primary() {
        let primary = mem();
        let fallback = mem();
        primary.write("k", b"in-primary").await.unwrap();
        fallback.write("k", b"in-fallback").await.unwrap();

        let overlay = OverlayBackend::new(vec![primary.clone(), fallback.clone()]);
        overlay.delete("k").await.unwrap();

        assert!(!primary.exists("k").await.unwrap());
        assert!(fallback.exists("k").await.unwrap());
        // Reading through overlay now falls through to fallback.
        assert_eq!(overlay.read("k").await.unwrap(), b"in-fallback");
    }

    #[tokio::test]
    async fn exists_returns_true_if_any_layer_has_key() {
        let primary = mem();
        let fallback = mem();
        fallback.write("only_in_fallback", b"").await.unwrap();
        let overlay = OverlayBackend::new(vec![primary, fallback]);

        assert!(overlay.exists("only_in_fallback").await.unwrap());
        assert!(!overlay.exists("nowhere").await.unwrap());
    }

    #[tokio::test]
    async fn single_layer_behaves_like_the_wrapped_backend() {
        let primary = mem();
        primary.write("k", b"v").await.unwrap();
        let overlay = OverlayBackend::new(vec![primary.clone()]);

        assert_eq!(overlay.read("k").await.unwrap(), b"v");
        overlay.write("k2", b"v2").await.unwrap();
        assert_eq!(primary.read("k2").await.unwrap(), b"v2");
    }
}
