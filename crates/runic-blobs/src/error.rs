//! Errors from a [`crate::BlobStore`].

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// The underlying storage layer failed (filesystem I/O, S3 error,
    /// network, …).
    #[error("storage error: {0}")]
    Storage(String),

    /// Serialization/deserialization of sidecar metadata failed.
    #[error("metadata error: {0}")]
    Metadata(#[from] serde_json::Error),

    /// Asked for a blob that doesn't exist for this tenant.
    #[error("blob '{id}' not found for tenant '{tenant}'")]
    NotFound { tenant: String, id: String },

    /// `BlobInput` had an empty or otherwise invalid mime type. We
    /// require this up front because provider adapters need it to
    /// encode correctly.
    #[error("invalid mime type: {0}")]
    InvalidMime(String),

    /// Caller-supplied id didn't match the actual content hash. We
    /// recompute on every `put` to guard against this.
    #[error("content hash mismatch (expected {expected}, computed {computed})")]
    HashMismatch { expected: String, computed: String },

    /// Method that a particular store doesn't support (e.g. `list`
    /// against an opaque KV without enumeration).
    #[error("operation not supported by this store: {0}")]
    Unsupported(String),
}

impl From<runic_storage_backend::StorageError> for BlobError {
    fn from(err: runic_storage_backend::StorageError) -> Self {
        match err {
            runic_storage_backend::StorageError::NotFound { key } => BlobError::Storage(format!(
                "underlying key '{key}' not found"
            )),
            other => BlobError::Storage(other.to_string()),
        }
    }
}
