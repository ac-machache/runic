//! `runic-substrate` — the agent's durable substrate (OpenFang's term).
//!
//! Two persistence concerns that share a database, keyed by
//! `(tenant, session_id)`:
//!
//! - **the session event log** — [`SessionStore`] persists every
//!   [`SessionEvent`](runic_state::SessionEvent) append-only and replays it
//!   back into an [`AgentState`](runic_state::AgentState). Plus full-text
//!   [`SessionStore::search`] over conversations (the [`SearchChatsTool`]).
//! - **media artifacts** — [`ArtifactStore`] holds the bytes (a user's PDF, a
//!   tool's screenshot); a message references them by id, the log stays lean.
//!
//! They live together because the Postgres `artifacts` table FKs to
//! `sessions` (delete a session → its artifacts cascade), and they share one
//! pool + migration set. Backends are pluggable: ship `Memory`/`Local`
//! artifact stores; the **`postgres`** feature adds `PostgresSessionStore` +
//! `PostgresArtifactStore`. This is the durable layer — separate from the
//! agent's working filesystem (`runic-filesystem`).

mod artifact_tool;
mod artifacts;
mod builders;
mod local;
mod memory;
mod replay;
mod sessions;
mod tool;

#[cfg(feature = "postgres")]
mod postgres;

pub use artifact_tool::ReadThreadArtifactTool;
pub use artifacts::{Artifact, ArtifactSource, ArtifactStore};
pub use builders::{Blobs, Sessions, blobs_local, blobs_memory, sessions_memory};
pub use local::LocalArtifactStore;
pub use memory::{MemoryArtifactStore, MemorySessionStore};
pub use replay::{replay_into_state, replay_messages};
pub use sessions::{ChatHit, SessionMeta, SessionStore, StoredEvent};
pub use tool::SearchChatsTool;

#[cfg(feature = "postgres")]
pub use builders::{blobs_postgres, sessions_postgres};
#[cfg(feature = "postgres")]
pub use postgres::{PostgresArtifactStore, PostgresSessionStore};

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
