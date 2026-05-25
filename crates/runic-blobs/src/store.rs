//! The `BlobStore` trait.

use async_trait::async_trait;
use runic_message_types::BlobRef;

use crate::error::BlobError;
use crate::metadata::{BlobInput, BlobMetadata};

/// Pluggable storage for binary blobs.
///
/// Implementations:
///   - [`crate::FileBlobStore`] — reference impl on `Arc<dyn StorageBackend>`
///   - User-written impls for S3, Postgres, R2, etc.
///
/// ## Identity
///
/// Every blob is keyed by `(tenant, content-hash)`. The hash is sha256
/// of the bytes, lowercase hex. Uploading the same bytes twice under
/// the same tenant is idempotent — both uploads return the same
/// [`BlobRef`], no extra storage used.
///
/// Tenants ARE isolated: alice uploading the same bytes as bob produces
/// the same `id` but separate storage entries (and `read("bob", id)`
/// from alice's side returns [`BlobError::NotFound`]).
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Upload bytes. Returns a [`BlobRef`] suitable to embed in a
    /// `ContentBlock::Blob` and ship around the conversation log.
    /// Idempotent — repeating the upload doesn't double-store.
    async fn put(&self, tenant: &str, input: BlobInput) -> Result<BlobRef, BlobError>;

    /// Read the bytes for a blob.
    async fn read(&self, tenant: &str, blob_id: &str) -> Result<Vec<u8>, BlobError>;

    /// Just the sidecar metadata — no bytes loaded.
    async fn metadata(&self, tenant: &str, blob_id: &str) -> Result<BlobMetadata, BlobError>;

    async fn exists(&self, tenant: &str, blob_id: &str) -> Result<bool, BlobError>;

    /// Best-effort delete. Returns Ok even if the blob didn't exist.
    async fn delete(&self, tenant: &str, blob_id: &str) -> Result<(), BlobError>;

    /// List metadata for all blobs owned by this tenant.
    /// Implementations may sort however they like (alphabetical by id
    /// is the convention) but MUST be deterministic.
    async fn list(&self, tenant: &str) -> Result<Vec<BlobMetadata>, BlobError>;
}
