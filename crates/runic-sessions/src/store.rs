//! The `SessionStore` trait.

use async_trait::async_trait;
use runic_agent_core::SessionEvent;

use crate::error::StoreError;

/// One stored event with its store-assigned monotonic sequence number.
///
/// The `seq` is unique per `(tenant, session_id)` and strictly
/// increasing in the order events were appended. Use it for tailing
/// (read events arrived after this point) and for stable ordering on
/// read.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub seq: u64,
    pub event: SessionEvent,
}

/// Pluggable storage for session event logs.
///
/// Implementations:
///   - [`crate::FileSessionStore`] — built on `runic_storage_backend`
///     (LocalFs / Memory / S3-via-overlay / …)
///   - User-written impls for Postgres, Redis, DynamoDB, sled, etc.
///
/// ### Identity
///
/// Every operation is scoped by `(tenant, session_id)`. The store
/// guarantees tenant isolation: a list for tenant `A` will never
/// return data from tenant `B`. How you obtain a `tenant` is your
/// application's choice — typically auth context (user id, org id,
/// JWT claim).
///
/// ### Sequence numbers
///
/// The store assigns the seq number on `append` (returned to caller).
/// Callers never manage ordering themselves. Reads always return events
/// in seq order.
///
/// ### Concurrency
///
/// `append` is expected to be atomic with respect to seq assignment:
/// two concurrent appends to the same `(tenant, session)` MUST receive
/// distinct, strictly-increasing seq values. Local-FS impls usually
/// achieve this with an in-process mutex; SQL impls with row locks.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Append one event. Store assigns the seq and returns it.
    async fn append(
        &self,
        tenant: &str,
        session_id: &str,
        event: &SessionEvent,
    ) -> Result<u64, StoreError>;

    /// Read every event for one session, in seq order.
    async fn read(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<StoredEvent>, StoreError>;

    /// Read events whose seq is strictly greater than `after_seq`.
    /// Useful for tailing: poll periodically with the last-seen seq.
    async fn read_after(
        &self,
        tenant: &str,
        session_id: &str,
        after_seq: u64,
    ) -> Result<Vec<StoredEvent>, StoreError>;

    /// List session ids for one tenant. Returns alphabetical order
    /// (implementation may sort however it likes, as long as it's
    /// deterministic).
    async fn list_sessions(&self, tenant: &str) -> Result<Vec<String>, StoreError>;

    /// List tenants known to this store.
    ///
    /// Optional — some backends (anything where tenants are an
    /// authentication concept, not a storage concept) can't enumerate.
    /// The default returns [`StoreError::Unsupported`].
    async fn list_tenants(&self) -> Result<Vec<String>, StoreError> {
        Err(StoreError::Unsupported("list_tenants".into()))
    }

    /// Remove a session and all its events. Best-effort.
    /// Returns Ok even if the session didn't exist.
    async fn delete_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<(), StoreError>;
}
