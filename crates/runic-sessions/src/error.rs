//! Errors from a [`crate::SessionStore`].

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Underlying storage failed (filesystem I/O, DB error, network, …).
    #[error("storage error: {0}")]
    Storage(String),

    /// Serialization/deserialization of an event failed. The event log
    /// format is JSON Lines; corrupt or malformed lines surface here.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// Asked for a session/tenant/etc. that doesn't exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// A method that some stores can't implement (e.g. `list_tenants`
    /// against a schemaless KV store with no enumeration support).
    #[error("operation not supported by this store: {0}")]
    Unsupported(String),

    /// Concurrent write conflict — two writers raced on the same
    /// `(tenant, session)`. Mostly informational; the caller usually
    /// retries.
    #[error("write conflict for tenant '{tenant}', session '{session}'")]
    Conflict { tenant: String, session: String },
}

impl From<runic_storage_backend::StorageError> for StoreError {
    fn from(err: runic_storage_backend::StorageError) -> Self {
        match err {
            runic_storage_backend::StorageError::NotFound { key } => StoreError::NotFound(key),
            other => StoreError::Storage(other.to_string()),
        }
    }
}
