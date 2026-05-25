//! Inputs and metadata types for [`crate::BlobStore`].

use serde::{Deserialize, Serialize};

/// What a caller passes to [`crate::BlobStore::put`].
#[derive(Debug, Clone)]
pub struct BlobInput {
    pub bytes: Vec<u8>,
    /// MIME type the uploader declared. Used by provider adapters when
    /// materializing the blob for the wire (image/png, application/pdf,
    /// etc.). Cannot be empty.
    pub mime: String,
    /// Optional original filename. Purely informational; never used for
    /// addressing or lookup. Useful for previews / debugging.
    pub name: Option<String>,
}

impl BlobInput {
    pub fn new(bytes: impl Into<Vec<u8>>, mime: impl Into<String>) -> Self {
        Self {
            bytes: bytes.into(),
            mime: mime.into(),
            name: None,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// Sidecar metadata persisted alongside the bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobMetadata {
    /// Content hash. Lowercase hex sha256 — same as [`BlobRef::id`].
    ///
    /// [`BlobRef::id`]: runic_message_types::BlobRef::id
    pub id: String,
    pub mime: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub tenant: String,
    pub uploaded_at: chrono::DateTime<chrono::Utc>,
}
