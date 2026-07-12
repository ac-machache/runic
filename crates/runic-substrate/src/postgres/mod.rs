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

const MIGRATIONS: [&str; 6] = [
    include_str!("../../migrations/0001_sessions.sql"),
    include_str!("../../migrations/0002_chat_search.sql"),
    include_str!("../../migrations/0003_artifacts.sql"),
    include_str!("../../migrations/0004_runs.sql"),
    include_str!("../../migrations/0005_run_inputs.sql"),
    include_str!("../../migrations/0006_thread_leases_and_signals.sql"),
];

const SCHEMA_VERSION: i32 = MIGRATIONS.len() as i32;

async fn current_version(conn: &mut sqlx::PgConnection) -> Result<Option<i32>> {
    let table: Option<String> = sqlx::query_scalar("SELECT to_regclass('substrate_schema')::text")
        .fetch_one(&mut *conn)
        .await
        .map_err(db)?;
    if table.is_none() {
        return Ok(None);
    }
    sqlx::query_scalar("SELECT version FROM substrate_schema WHERE id = 1")
        .fetch_optional(&mut *conn)
        .await
        .map_err(db)
}

/// Apply the substrate schema (idempotent — `CREATE … IF NOT EXISTS`).
/// Runs in order so the artifacts FK to `sessions` resolves.
///
/// A boot against a current schema must not touch DDL at all: even
/// `CREATE INDEX IF NOT EXISTS` on an existing index takes a ShareLock, which
/// deadlocks against live multi-statement writers holding row locks in the
/// opposite table order. The applied version is stamped in `substrate_schema`,
/// so such boots reduce to one SELECT. Concurrent first-boots (several app
/// instances, a parallel test suite) are serialized by a transaction-scoped
/// advisory lock and re-check the stamp under it.
pub(super) async fn migrate(pool: &PgPool) -> Result<()> {
    {
        let mut conn = pool.acquire().await.map_err(db)?;
        if current_version(&mut conn).await? == Some(SCHEMA_VERSION) {
            return Ok(());
        }
    }
    let mut tx = pool.begin().await.map_err(db)?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    if current_version(&mut tx).await? == Some(SCHEMA_VERSION) {
        return Ok(());
    }
    for sql in MIGRATIONS {
        sqlx::raw_sql(sql).execute(&mut *tx).await.map_err(db)?;
    }
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS substrate_schema (id int PRIMARY KEY CHECK (id = 1), version int NOT NULL)",
    )
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    sqlx::query(
        "INSERT INTO substrate_schema (id, version) VALUES (1, $1)
         ON CONFLICT (id) DO UPDATE SET version = EXCLUDED.version",
    )
    .bind(SCHEMA_VERSION)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(())
}
