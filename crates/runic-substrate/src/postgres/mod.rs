//! Postgres backends (feature `postgres`): the session store + the artifact
//! metadata index, sharing one pool, schema, and migration set.

mod artifacts;
mod sessions;

pub use artifacts::PostgresArtifactStore;
pub use sessions::PostgresSessionStore;

use sqlx::PgPool;

use crate::{Error, Result};

pub(super) fn db(e: sqlx::Error) -> Error {
    Error::Database(e.to_string())
}

pub(super) fn serde(e: serde_json::Error) -> Error {
    Error::Serde(e.to_string())
}

/// Apply the substrate schema (idempotent — `CREATE … IF NOT EXISTS`).
/// Runs in order so the artifacts FK to `sessions` resolves.
pub(super) async fn migrate(pool: &PgPool) -> Result<()> {
    for sql in [
        include_str!("../../migrations/0001_sessions.sql"),
        include_str!("../../migrations/0002_chat_search.sql"),
        include_str!("../../migrations/0003_artifacts.sql"),
    ] {
        sqlx::raw_sql(sql).execute(pool).await.map_err(db)?;
    }
    Ok(())
}
