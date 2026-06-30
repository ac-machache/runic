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

/// Arbitrary fixed key for the migration advisory lock (ascii "runicsub").
const MIGRATION_LOCK_KEY: i64 = 0x72756e6963737562_u64 as i64;

/// Apply the substrate schema (idempotent — `CREATE … IF NOT EXISTS`).
/// Runs in order so the artifacts FK to `sessions` resolves.
///
/// Concurrent migrators (several app instances booting at once, or a parallel
/// test suite) otherwise race on the `CREATE … IF NOT EXISTS` DDL — Postgres
/// can trip `pg_type_typname_nsp_index` or deadlock. A transaction-scoped
/// advisory lock serializes them; it auto-releases on commit.
pub(super) async fn migrate(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await.map_err(db)?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    for sql in [
        include_str!("../../migrations/0001_sessions.sql"),
        include_str!("../../migrations/0002_chat_search.sql"),
        include_str!("../../migrations/0003_artifacts.sql"),
    ] {
        sqlx::raw_sql(sql).execute(&mut *tx).await.map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    Ok(())
}
