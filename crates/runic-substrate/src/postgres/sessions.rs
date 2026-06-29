//! Postgres-backed [`SessionStore`].
//!
//! Event log in `session_events` (append-only, `(tenant, session_id, seq)`),
//! per-session counter + metadata in `sessions`, and a full-text projection of
//! conversational messages in `chat_messages` (for `search`).

use async_trait::async_trait;
use chrono::{Duration, Utc};
use sqlx::{PgPool, Row};

use runic_state::SessionEvent;
use runic_types::Role;

use super::{db, migrate, serde};
use crate::sessions::event_at;
use crate::{ChatHit, Result, SessionMeta, SessionStore, StoredEvent};

/// A Postgres session store over a connection pool.
pub struct PostgresSessionStore {
    pool: PgPool,
}

impl PostgresSessionStore {
    /// Connect to `database_url` and run migrations.
    pub async fn connect(database_url: &str) -> Result<Self> {
        let pool = PgPool::connect(database_url).await.map_err(db)?;
        Self::from_pool(pool).await
    }

    /// Build from an existing pool (e.g. one shared with the app) and migrate.
    pub async fn from_pool(pool: PgPool) -> Result<Self> {
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

fn event_kind(e: &SessionEvent) -> &'static str {
    match e {
        SessionEvent::RunStart { .. } => "RunStart",
        SessionEvent::RunEnd { .. } => "RunEnd",
        SessionEvent::Message { .. } => "Message",
        SessionEvent::TurnBoundary { .. } => "TurnBoundary",
        SessionEvent::HookRan { .. } => "HookRan",
        SessionEvent::StateSnapshot { .. } => "StateSnapshot",
    }
}

fn rows_to_events(rows: Vec<sqlx::postgres::PgRow>) -> Result<Vec<StoredEvent>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let seq: i64 = row.try_get("seq").map_err(db)?;
        let json: serde_json::Value = row.try_get("event").map_err(db)?;
        let event = serde_json::from_value(json).map_err(serde)?;
        out.push(StoredEvent {
            seq: seq as u64,
            event,
        });
    }
    Ok(out)
}

/// Write one event inside an open transaction: bump the session seq, insert the
/// event, and project message text into the search index. Returns the seq.
async fn write_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    session_id: &str,
    event: &SessionEvent,
) -> Result<i64> {
    let at = event_at(event);
    let kind = event_kind(event);
    let run_id = event.run_id().to_string();
    let json = serde_json::to_value(event).map_err(serde)?;

    let seq: i64 = sqlx::query_scalar(
        "INSERT INTO sessions (tenant, session_id, last_seq, event_count, last_activity)
         VALUES ($1, $2, 1, 1, $3)
         ON CONFLICT (tenant, session_id) DO UPDATE
           SET last_seq = sessions.last_seq + 1,
               event_count = sessions.event_count + 1,
               last_activity = EXCLUDED.last_activity
         RETURNING last_seq",
    )
    .bind(tenant)
    .bind(session_id)
    .bind(at)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;

    sqlx::query(
        "INSERT INTO session_events (tenant, session_id, seq, kind, run_id, at, event)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(tenant)
    .bind(session_id)
    .bind(seq)
    .bind(kind)
    .bind(&run_id)
    .bind(at)
    .bind(&json)
    .execute(&mut **tx)
    .await
    .map_err(db)?;

    if let SessionEvent::Message { msg, .. } = event {
        let role = match msg.role {
            Role::User => Some("user"),
            Role::Assistant => Some("assistant"),
            Role::System => None,
        };
        if let Some(role) = role {
            let text = msg.content.text_content();
            if !text.trim().is_empty() {
                sqlx::query(
                    "INSERT INTO chat_messages (tenant, session_id, seq, role, text, at)
                     VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
                )
                .bind(tenant)
                .bind(session_id)
                .bind(seq)
                .bind(role)
                .bind(&text)
                .bind(at)
                .execute(&mut **tx)
                .await
                .map_err(db)?;
            }
        }
    }

    Ok(seq)
}

#[async_trait]
impl SessionStore for PostgresSessionStore {
    async fn append(&self, tenant: &str, session_id: &str, event: &SessionEvent) -> Result<u64> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let seq = write_event(&mut tx, tenant, session_id, event).await?;
        tx.commit().await.map_err(db)?;
        Ok(seq as u64)
    }

    async fn append_batch(
        &self,
        tenant: &str,
        session_id: &str,
        events: &[SessionEvent],
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(db)?;
        for event in events {
            write_event(&mut tx, tenant, session_id, event).await?;
        }
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn read(&self, tenant: &str, session_id: &str) -> Result<Vec<StoredEvent>> {
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
    ) -> Result<Vec<StoredEvent>> {
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

    async fn read_run_after(
        &self,
        tenant: &str,
        session_id: &str,
        run_id: &str,
        after_seq: u64,
    ) -> Result<Vec<StoredEvent>> {
        let rows = sqlx::query(
            "SELECT seq, event FROM session_events
             WHERE tenant = $1 AND session_id = $2 AND run_id = $3 AND seq > $4 ORDER BY seq",
        )
        .bind(tenant)
        .bind(session_id)
        .bind(run_id)
        .bind(after_seq as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows_to_events(rows)
    }

    async fn list_sessions(&self, tenant: &str) -> Result<Vec<SessionMeta>> {
        let rows = sqlx::query(
            "SELECT session_id, label, event_count, created_at, last_activity
             FROM sessions WHERE tenant = $1 ORDER BY last_activity DESC",
        )
        .bind(tenant)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(SessionMeta {
                session_id: row.try_get("session_id").map_err(db)?,
                label: row.try_get("label").map_err(db)?,
                event_count: row.try_get::<i64, _>("event_count").map_err(db)? as u64,
                created_at: row.try_get("created_at").map_err(db)?,
                last_activity: row.try_get("last_activity").map_err(db)?,
            });
        }
        Ok(out)
    }

    async fn delete_session(&self, tenant: &str, session_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE tenant = $1 AND session_id = $2")
            .bind(tenant)
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn search(
        &self,
        tenant: &str,
        query: &str,
        limit: usize,
        exclude_session: Option<&str>,
    ) -> Result<Vec<ChatHit>> {
        let rows = sqlx::query(
            "SELECT session_id, seq, role, at,
                    ts_headline('english', text, q) AS snippet
             FROM chat_messages, websearch_to_tsquery('english', $2) q
             WHERE tenant = $1 AND tsv @@ q
               AND ($3::text IS NULL OR session_id <> $3)
             ORDER BY ts_rank(tsv, q) DESC
             LIMIT $4",
        )
        .bind(tenant)
        .bind(query)
        .bind(exclude_session)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(ChatHit {
                session_id: row.try_get("session_id").map_err(db)?,
                seq: row.try_get::<i64, _>("seq").map_err(db)? as u64,
                role: row.try_get("role").map_err(db)?,
                snippet: row.try_get("snippet").map_err(db)?,
                at: row.try_get("at").map_err(db)?,
            });
        }
        Ok(out)
    }

    async fn cleanup_stale(&self, ttl: Duration) -> Result<u64> {
        let cutoff = Utc::now() - ttl;
        let res = sqlx::query("DELETE FROM sessions WHERE last_activity < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(res.rows_affected())
    }

    async fn list_tenants(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT DISTINCT tenant FROM sessions ORDER BY tenant")
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.into_iter()
            .map(|r| r.try_get::<String, _>("tenant").map_err(db))
            .collect()
    }
}
