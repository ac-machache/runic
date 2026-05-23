//! Errors a `StorageBackend` operation can produce.
//!
//! Variants are deliberately granular so callers can branch on the
//! specific failure mode (e.g. `NotFound` vs `PermissionDenied`) without
//! string-matching error messages.

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("not found: {key}")]
    NotFound { key: String },

    #[error("already exists: {key}")]
    AlreadyExists { key: String },

    #[error("permission denied: {key}")]
    PermissionDenied { key: String },

    /// The backend doesn't support this operation (e.g. a read-only mirror
    /// can't write). String is a human-readable explanation.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// Key violates the backend's rules (e.g. contains `..` in LocalFs).
    #[error("invalid key: {0}")]
    InvalidKey(String),

    /// Catch-all I/O error.
    #[error("io error: {0}")]
    Io(String),

    /// UTF-8 decode failure in `read_to_string` (or similar).
    #[error("decode error: {0}")]
    Decode(String),
}

impl StorageError {
    pub fn not_found(key: impl Into<String>) -> Self {
        Self::NotFound { key: key.into() }
    }

    pub fn invalid_key(msg: impl Into<String>) -> Self {
        Self::InvalidKey(msg.into())
    }

    pub fn io(msg: impl std::fmt::Display) -> Self {
        Self::Io(msg.to_string())
    }
}
