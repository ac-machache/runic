//! Postgres-backed [`SessionStore`] (feature `postgres`).
//!
//! One append-only `session_events` table keyed `(tenant, session_id, seq)`.
//! `tenant` is the multi-tenancy boundary (your `org_id`); `event` is the
//! `SessionEvent` stored as JSONB, so `RunEnd`'s usage/model/provider land
//! in the row with no schema changes. Drops into the server via the
//! `SessionStore` trait — same as [`crate::FileSessionStore`].

use async_trait::async_trait;
use runic_agent_core::SessionEvent;
use sqlx::{postgres::PgRow, PgPool, Row};

use crate::error::StoreError;
use crate::store::{SessionStore, StoredEvent};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS session_events (
    tenant     TEXT   NOT NULL,
    session_id TEXT   NOT NULL,
    seq        BIGINT NOT NULL,
    event      JSONB  NOT NULL,
    PRIMARY KEY (tenant, session_id, seq)
);";

pub struct PostgresSessionStore {
    pool: PgPool,
}

impl PostgresSessionStore {
    /// Connect to `database_url` and ensure the schema exists.
    pub async fn connect(database_url: &str) -> Result<Self, StoreError> {
        let pool = PgPool::connect(database_url).await.map_err(db)?;
        Self::from_pool(pool).await
    }

    /// Build from an existing pool (e.g. one shared with the rest of the app).
    pub async fn from_pool(pool: PgPool) -> Result<Self, StoreError> {
        sqlx::query(SCHEMA).execute(&pool).await.map_err(db)?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl SessionStore for PostgresSessionStore {
    async fn append(
        &self,
        tenant: &str,
        session_id: &str,
        event: &SessionEvent,
    ) -> Result<u64, StoreError> {
        let value = serde_json::to_value(event)?; // -> StoreError::Serde via `?`
        // seq = next per (tenant, session). Safe because runs serialize per
        // session; the subquery + PK make a racing duplicate fail loudly
        // rather than silently overwrite.
        let row = sqlx::query(
            "INSERT INTO session_events (tenant, session_id, seq, event)
             VALUES ($1, $2,
                (SELECT COALESCE(MAX(seq), 0) + 1 FROM session_events
                 WHERE tenant = $1 AND session_id = $2),
                $3)
             RETURNING seq",
        )
        .bind(tenant)
        .bind(session_id)
        .bind(value)
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        let seq: i64 = row.try_get("seq").map_err(db)?;
        Ok(seq as u64)
    }

    async fn read(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let rows = sqlx::query(
            "SELECT seq, event FROM session_events
             WHERE tenant = $1 AND session_id = $2 ORDER BY seq",
        )
        .bind(tenant)
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows_to_events(rows)
    }

    async fn read_after(
        &self,
        tenant: &str,
        session_id: &str,
        after_seq: u64,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let rows = sqlx::query(
            "SELECT seq, event FROM session_events
             WHERE tenant = $1 AND session_id = $2 AND seq > $3 ORDER BY seq",
        )
        .bind(tenant)
        .bind(session_id)
        .bind(after_seq as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows_to_events(rows)
    }

    async fn list_sessions(&self, tenant: &str) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query(
            "SELECT DISTINCT session_id FROM session_events
             WHERE tenant = $1 ORDER BY session_id",
        )
        .bind(tenant)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.into_iter()
            .map(|r| r.try_get::<String, _>("session_id").map_err(db))
            .collect()
    }

    async fn list_tenants(&self) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query("SELECT DISTINCT tenant FROM session_events ORDER BY tenant")
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.into_iter()
            .map(|r| r.try_get::<String, _>("tenant").map_err(db))
            .collect()
    }

    async fn delete_session(&self, tenant: &str, session_id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM session_events WHERE tenant = $1 AND session_id = $2")
            .bind(tenant)
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }
}

fn rows_to_events(rows: Vec<PgRow>) -> Result<Vec<StoredEvent>, StoreError> {
    rows.into_iter()
        .map(|r| {
            let seq: i64 = r.try_get("seq").map_err(db)?;
            let value: serde_json::Value = r.try_get("event").map_err(db)?;
            Ok(StoredEvent {
                seq: seq as u64,
                event: serde_json::from_value(value)?,
            })
        })
        .collect()
}

fn db<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Storage(e.to_string())
}
