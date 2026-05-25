//! [`FileBlobStore`] — content-addressed [`BlobStore`] on top of any
//! [`runic_storage_backend::StorageBackend`].
//!
//! On-disk layout:
//!
//! ```text
//! {root}/{tenant}/{hash[:2]}/{hash}/data
//! {root}/{tenant}/{hash[:2]}/{hash}/meta.json
//! ```
//!
//! The 2-character hash prefix avoids one giant flat directory when
//! you accumulate millions of blobs.
//!
//! Sidecar `meta.json` lets us return [`crate::BlobMetadata`] without
//! reading the bytes — useful for previews and list operations.
//!
//! `put` is idempotent thanks to content addressing: the same bytes
//! always hash to the same id, so re-uploading is a no-op (we still
//! recompute the hash to verify, but the write is overwritten with
//! identical content, which is fine).

use async_trait::async_trait;
use runic_message_types::BlobRef;
use runic_storage_backend::StorageBackend;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::error::BlobError;
use crate::metadata::{BlobInput, BlobMetadata};
use crate::store::BlobStore;

const DEFAULT_ROOT: &str = "blobs";

pub struct FileBlobStore {
    storage: Arc<dyn StorageBackend>,
    root: String,
}

impl FileBlobStore {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self::with_root(storage, DEFAULT_ROOT)
    }

    pub fn with_root(storage: Arc<dyn StorageBackend>, root: impl Into<String>) -> Self {
        Self {
            storage,
            root: root.into(),
        }
    }

    fn data_path(&self, tenant: &str, blob_id: &str) -> String {
        let prefix = &blob_id[..2.min(blob_id.len())];
        format!("{}/{tenant}/{prefix}/{blob_id}/data", self.root)
    }

    fn meta_path(&self, tenant: &str, blob_id: &str) -> String {
        let prefix = &blob_id[..2.min(blob_id.len())];
        format!("{}/{tenant}/{prefix}/{blob_id}/meta.json", self.root)
    }

    fn tenant_root(&self, tenant: &str) -> String {
        format!("{}/{tenant}", self.root)
    }
}

/// Compute the lowercase hex sha256 of `bytes`.
fn content_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex::encode(digest)
}

#[async_trait]
impl BlobStore for FileBlobStore {
    async fn put(&self, tenant: &str, input: BlobInput) -> Result<BlobRef, BlobError> {
        if input.mime.trim().is_empty() {
            return Err(BlobError::InvalidMime("mime must not be empty".into()));
        }
        if tenant.is_empty() {
            return Err(BlobError::Storage("tenant must not be empty".into()));
        }

        let id = content_hash(&input.bytes);
        let size = input.bytes.len() as u64;
        let data_path = self.data_path(tenant, &id);
        let meta_path = self.meta_path(tenant, &id);

        // Write the bytes (overwrite — content is identical so this is
        // a no-op if the file already exists).
        self.storage.write(&data_path, &input.bytes).await?;

        let metadata = BlobMetadata {
            id: id.clone(),
            mime: input.mime.clone(),
            size,
            name: input.name.clone(),
            tenant: tenant.to_string(),
            uploaded_at: chrono::Utc::now(),
        };
        let serialized = serde_json::to_vec(&metadata)?;
        self.storage.write(&meta_path, &serialized).await?;

        Ok(BlobRef {
            id,
            mime: input.mime,
            size,
            name: input.name,
        })
    }

    async fn read(&self, tenant: &str, blob_id: &str) -> Result<Vec<u8>, BlobError> {
        let path = self.data_path(tenant, blob_id);
        match self.storage.read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(runic_storage_backend::StorageError::NotFound { .. }) => Err(BlobError::NotFound {
                tenant: tenant.to_string(),
                id: blob_id.to_string(),
            }),
            Err(other) => Err(other.into()),
        }
    }

    async fn metadata(&self, tenant: &str, blob_id: &str) -> Result<BlobMetadata, BlobError> {
        let path = self.meta_path(tenant, blob_id);
        let raw = match self.storage.read(&path).await {
            Ok(b) => b,
            Err(runic_storage_backend::StorageError::NotFound { .. }) => {
                return Err(BlobError::NotFound {
                    tenant: tenant.to_string(),
                    id: blob_id.to_string(),
                })
            }
            Err(other) => return Err(other.into()),
        };
        let meta: BlobMetadata = serde_json::from_slice(&raw)?;
        Ok(meta)
    }

    async fn exists(&self, tenant: &str, blob_id: &str) -> Result<bool, BlobError> {
        let path = self.data_path(tenant, blob_id);
        Ok(self.storage.exists(&path).await?)
    }

    async fn delete(&self, tenant: &str, blob_id: &str) -> Result<(), BlobError> {
        // Best-effort: ignore NotFound on either file.
        match self.storage.delete(&self.data_path(tenant, blob_id)).await {
            Ok(()) | Err(runic_storage_backend::StorageError::NotFound { .. }) => {}
            Err(other) => return Err(other.into()),
        }
        match self.storage.delete(&self.meta_path(tenant, blob_id)).await {
            Ok(()) | Err(runic_storage_backend::StorageError::NotFound { .. }) => {}
            Err(other) => return Err(other.into()),
        }
        Ok(())
    }

    async fn list(&self, tenant: &str) -> Result<Vec<BlobMetadata>, BlobError> {
        // Walk the tenant tree iteratively. Hierarchical backends
        // (LocalFs) return Directory entries we recurse into; flat-KV
        // backends (Memory, S3) return File entries with full paths we
        // pick up directly. The walk handles both.
        use runic_storage_backend::EntryKind;

        let mut to_visit = vec![self.tenant_root(tenant)];
        let mut ids = BTreeSet::new();

        while let Some(prefix) = to_visit.pop() {
            let entries = self.storage.list(&prefix).await?;
            for entry in entries {
                match entry.kind {
                    EntryKind::File if entry.key.ends_with("/meta.json") => {
                        // The blob id is the leaf directory containing
                        // meta.json: "{root}/{tenant}/{aa}/{full_hash}/meta.json"
                        if let Some(parent) = std::path::Path::new(&entry.key).parent()
                            && let Some(id) =
                                parent.file_name().and_then(|n| n.to_str())
                        {
                            ids.insert(id.to_string());
                        }
                    }
                    EntryKind::Directory => {
                        to_visit.push(entry.key);
                    }
                    _ => {}
                }
            }
        }

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            match self.metadata(tenant, &id).await {
                Ok(m) => out.push(m),
                Err(BlobError::NotFound { .. }) => {} // raced with a delete; skip
                Err(other) => return Err(other),
            }
        }
        Ok(out)
    }
}

impl std::fmt::Debug for FileBlobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileBlobStore")
            .field("root", &self.root)
            .finish()
    }
}
