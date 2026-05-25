//! `runic-blobs` — pluggable, content-addressed blob storage.
//!
//! Users upload files (images, PDFs, audio, anything) into a [`BlobStore`].
//! The store assigns a content-addressed id (sha256 of the bytes), so
//! identical uploads dedupe automatically. The conversation log carries
//! only a small [`BlobRef`] (id + mime + size + name); the actual bytes
//! live in the store until needed.
//!
//! ## Why blobs are a separate concept from `runic-sessions`
//!
//! Sessions hold ordered, append-only events that describe an agent's
//! lifecycle. Blobs hold large, immutable, content-addressed bytes that
//! may be referenced by many sessions across many tenants. Different
//! consistency semantics, different size profiles, different access
//! patterns → different abstraction.
//!
//! ## The shape
//!
//! - [`BlobStore`] — the trait. Implementations: local file system,
//!   S3, Postgres-with-bytea, R2, anything you write.
//! - [`FileBlobStore`] — reference impl on `Arc<dyn StorageBackend>`.
//!   Content-addressed; durable; deduplicated.
//! - [`BlobInput`] — what you upload (bytes + mime + optional name).
//! - [`BlobRef`] (re-exported from `runic-message-types`) — the
//!   reference that lives in messages.
//! - [`BlobMetadata`] — sidecar info kept alongside the bytes.
//!
//! ## Multi-tenant by design
//!
//! Every method takes `tenant: &str`. A blob uploaded under tenant
//! `alice` is not visible (or readable) under tenant `bob`, even
//! though both might have happened to upload the same bytes. The
//! content hash is the same; the storage paths are isolated.
//!
//! ## Provider materialization
//!
//! When a message contains `ContentBlock::Blob(BlobRef)`, the provider
//! adapter is responsible for fetching the bytes (via a
//! [`BlobResolver`]) and encoding them in the provider's expected
//! format — Anthropic's base64 image block, Gemini's `inlineData`, etc.
//! See [`crate::resolver`].

pub mod error;
pub mod file;
pub mod metadata;
pub mod provider;
pub mod resolver;
pub mod store;

pub use error::BlobError;
pub use file::FileBlobStore;
pub use metadata::{BlobInput, BlobMetadata};
pub use provider::BlobMaterializingProvider;
pub use resolver::{BlobResolver, BlobStoreResolver};
pub use runic_message_types::BlobRef;
pub use store::BlobStore;
