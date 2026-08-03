//! `runic-store` — the agent's durable store.
//!
//! Two persistence concerns that share a database, keyed by
//! `(tenant, session_id)`:
//!
//! - **the session event log** — [`SessionStore`] persists every
//!   [`SessionEvent`](runic_state::SessionEvent) append-only and replays it
//!   back into an [`AgentState`](runic_state::AgentState). Plus full-text
//!   [`SessionStore::search`] over conversations.
//! - **media artifacts** — [`ArtifactStore`] holds the bytes (a user's PDF, a
//!   tool's screenshot); a message references them by id, the log stays lean.
//!
//! They live together because the Postgres `artifacts` table FKs to
//! `sessions` (delete a session → its artifacts cascade), and they share one
//! pool + migration set. Artifact bytes are held by a single [`ArtifactStore`]
//! over an OpenDAL operator — memory, local disk, S3, GCS or Azure Blob — and
//! the **`postgres`** feature adds `PostgresSessionStore` plus an indexed
//! `PostgresArtifactStore` layered over any of them. This is the durable layer
//! — separate from the agent's working filesystem.

pub mod artifacts;
mod event;
mod memory;
mod persister;
mod replay;
mod sessions;
mod store;
pub mod timeline;

#[cfg(feature = "postgres")]
mod postgres;

#[cfg(feature = "sqlite")]
mod sqlite;

pub use artifacts::{Artifact, ArtifactFiles, ArtifactSource, ArtifactStore};
pub use event::{SessionEvent, project};
pub use memory::MemorySessionStore;
pub use persister::{
    PersistDrain, PersistHandle, PersistPipe, RetryPolicy, StoreSubSession,
    attach as attach_persister, persist_channel, spawn_persist,
};
pub use replay::{replay_into_state, replay_messages};
pub use sessions::{ChatHit, SessionMeta, SessionScope, SessionStore, StoredEvent};
pub use store::{DEFAULT_ARTIFACT_DIR, Store};
pub use timeline::{DelegationTrace, RunTrace, ToolTrace, TraceStatus, TurnTrace};

#[cfg(feature = "postgres")]
pub use postgres::{PostgresArtifactStore, PostgresSessionStore};

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteSessionStore;

/// One error type for the whole substrate — sessions and artifacts alike.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("operation unsupported by this backend: {0}")]
    Unsupported(String),
    #[error("database error: {0}")]
    Database(String),
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("backend error: {0}")]
    Backend(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;
