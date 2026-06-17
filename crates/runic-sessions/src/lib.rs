//! `runic-sessions` — pluggable session storage for runic agents.
//!
//! The agent broadcasts every [`runic_agent_core::SessionEvent`] via its
//! `subscribe_events()` channel. This crate consumes that stream and
//! persists it through a [`SessionStore`] of the caller's choosing.
//!
//! ## The two pieces
//!
//! 1. **The trait** — [`SessionStore`]. Defines append / read / list /
//!    delete operations keyed by `(tenant, session_id)`. One trait, many
//!    backends.
//!
//! 2. **The glue** — [`spawn_persister`] subscribes to the agent's event
//!    stream and pipes everything into a `SessionStore`. The agent stays
//!    completely unaware of persistence.
//!
//! ## Reference implementation
//!
//! [`FileSessionStore`] is built on `Arc<dyn StorageBackend>`, which means
//! a single implementation works against LocalFs, in-memory, S3, or
//! anything else implementing `StorageBackend`. Adding a new physical
//! backend (Postgres, Redis, etc.) means writing a new `SessionStore`
//! impl, not a new file-storage adapter.
//!
//! ## Multi-tenant by design
//!
//! Every method takes `tenant: &str` as the first scoping key. In a
//! single-user deployment, pass `"default"`. In a multi-tenant SaaS,
//! pass the authenticated user/org id. The store guarantees
//! tenant-isolated views — `list_sessions("alice")` will never return
//! Bob's sessions.
//!
//! ## Composability via decorators
//!
//! `SessionStore` is a trait, so you can wrap one in another:
//!
//! ```ignore
//! let store = Arc::new(FileSessionStore::new(storage));
//! let store = Arc::new(AuditingSessionStore::new(store, audit_sink));
//! let store = Arc::new(CachingSessionStore::new(store, lru_capacity));
//! ```
//!
//! Same pattern as `tower::Layer` or our own `ContextEngine` decorators.

pub mod error;
pub mod file;
pub mod persister;
#[cfg(feature = "postgres")]
pub mod postgres;
pub mod replay;
pub mod store;

pub use error::StoreError;
pub use file::FileSessionStore;
pub use persister::{spawn_persister, PersisterHandle};
#[cfg(feature = "postgres")]
pub use postgres::PostgresSessionStore;
pub use replay::{replay_into_state, replay_messages};
pub use store::{SessionStore, StoredEvent};
