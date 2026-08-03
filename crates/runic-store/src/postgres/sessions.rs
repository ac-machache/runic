//! Postgres-backed [`SessionStore`].
//!
//! Event log in `runic.events` (append-only, `(tenant, session_id, seq)`),
//! per-session counter + metadata in `runic.sessions`, and a full-text
//! projection of conversational messages in `runic.chats` (for `search`).

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sqlx::{PgPool, Row};

use crate::SessionEvent;
use runic_types::Role;

use super::{db, migrate, serde};
use crate::sessions::{SessionScope, event_at};
use crate::{ChatHit, Error, Result, SessionMeta, SessionStore, StoredEvent};

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
        SessionEvent::TurnEnd { .. } => "TurnEnd",
        SessionEvent::ToolStarted { .. } => "ToolStarted",
        SessionEvent::ToolFinished { .. } => "ToolFinished",
        SessionEvent::DelegationStarted { .. } => "DelegationStarted",
        SessionEvent::DelegationFinished { .. } => "DelegationFinished",
        SessionEvent::HookFired { .. } => "HookFired",
        SessionEvent::StateSnapshot { .. } => "StateSnapshot",
        SessionEvent::TaskSpawned { .. } => "TaskSpawned",
        SessionEvent::TaskFinished { .. } => "TaskFinished",
        SessionEvent::StateUpdated { .. } => "StateUpdated",
        SessionEvent::ToolDeferred { .. } => "ToolDeferred",
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

fn row_to_meta(row: sqlx::postgres::PgRow) -> Result<SessionMeta> {
    Ok(SessionMeta {
        session_id: row.try_get("session_id").map_err(db)?,
        label: row.try_get("label").map_err(db)?,
        event_count: row.try_get::<i64, _>("event_count").map_err(db)? as u64,
        created_at: row.try_get("created_at").map_err(db)?,
        last_activity: row.try_get("last_activity").map_err(db)?,
        agent: row.try_get("agent").map_err(db)?,
        parent_session: row.try_get("parent_session").map_err(db)?,
        run_count: row.try_get::<i64, _>("run_count").map_err(db)? as u64,
        errored_runs: row.try_get::<i64, _>("errored_runs").map_err(db)? as u64,
        input_tokens: row.try_get::<i64, _>("input_tokens").map_err(db)? as u64,
        output_tokens: row.try_get::<i64, _>("output_tokens").map_err(db)? as u64,
        last_run_status: row.try_get("last_run_status").map_err(db)?,
        last_run_at: row.try_get("last_run_at").map_err(db)?,
    })
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
    let delta = crate::sessions::summary_delta(event);

    let seq: i64 = sqlx::query_scalar(
        "INSERT INTO runic.sessions (tenant, session_id, last_seq, event_count, last_activity,
                               run_count, errored_runs, input_tokens, output_tokens,
                               last_run_status, last_run_at)
         VALUES ($1, $2, 1, 1, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (tenant, session_id) DO UPDATE
           SET last_seq = sessions.last_seq + 1,
               event_count = sessions.event_count + 1,
               last_activity = EXCLUDED.last_activity,
               run_count = sessions.run_count + EXCLUDED.run_count,
               errored_runs = sessions.errored_runs + EXCLUDED.errored_runs,
               input_tokens = sessions.input_tokens + EXCLUDED.input_tokens,
               output_tokens = sessions.output_tokens + EXCLUDED.output_tokens,
               last_run_status = COALESCE(EXCLUDED.last_run_status, sessions.last_run_status),
               last_run_at = COALESCE(EXCLUDED.last_run_at, sessions.last_run_at)
         RETURNING last_seq",
    )
    .bind(tenant)
    .bind(session_id)
    .bind(at)
    .bind(delta.runs as i64)
    .bind(delta.errored as i64)
    .bind(delta.input_tokens as i64)
    .bind(delta.output_tokens as i64)
    .bind(delta.last_run_status)
    .bind(delta.last_run_at)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;

    sqlx::query(
        "INSERT INTO runic.events (tenant, session_id, seq, kind, run_id, at, event)
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
                    "INSERT INTO runic.chats (tenant, session_id, seq, role, text, at)
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

    async fn append_batch_strict(
        &self,
        tenant: &str,
        session_id: &str,
        events: &[SessionEvent],
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(db)?;
        let exists: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM runic.sessions WHERE tenant = $1 AND session_id = $2 FOR UPDATE",
        )
        .bind(tenant)
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        if exists.is_none() {
            return Err(Error::NotFound(format!("session {session_id}")));
        }
        for event in events {
            write_event(&mut tx, tenant, session_id, event).await?;
        }
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn read(&self, tenant: &str, session_id: &str) -> Result<Vec<StoredEvent>> {
        let rows = sqlx::query(
            "SELECT seq, event FROM runic.events
             WHERE tenant = $1 AND session_id = $2 ORDER BY seq",
        )
        .bind(tenant)
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows_to_events(rows)
    }

    async fn read_tail(&self, tenant: &str, session_id: &str) -> Result<Vec<StoredEvent>> {
        let rows = sqlx::query(
            "SELECT seq, event FROM runic.events
             WHERE tenant = $1 AND session_id = $2
               AND seq >= COALESCE((
                 SELECT MAX(seq) FROM runic.events
                 WHERE tenant = $1 AND session_id = $2 AND kind = 'StateSnapshot'
               ), 0)
             ORDER BY seq",
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
            "SELECT seq, event FROM runic.events
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
            "SELECT seq, event FROM runic.events
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

    async fn read_after_limited(
        &self,
        tenant: &str,
        session_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        let rows = sqlx::query(
            "SELECT seq, event FROM runic.events
             WHERE tenant = $1 AND session_id = $2 AND seq > $3 ORDER BY seq LIMIT $4",
        )
        .bind(tenant)
        .bind(session_id)
        .bind(after_seq as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows_to_events(rows)
    }

    async fn list_sessions_page(
        &self,
        tenant: &str,
        after: Option<(DateTime<Utc>, String)>,
        limit: usize,
        scope: SessionScope,
    ) -> Result<Vec<SessionMeta>> {
        let (scope_kind, scope_parent) = match &scope {
            SessionScope::Roots => ("roots", None),
            SessionScope::ChildrenOf(parent) => ("children", Some(parent.clone())),
            SessionScope::All => ("all", None),
        };
        let rows = match after {
            Some((at, id)) => sqlx::query(
                "SELECT session_id, label, event_count, created_at, last_activity,
                        agent, parent_session,
                        run_count, errored_runs, input_tokens, output_tokens,
                        last_run_status, last_run_at
                 FROM runic.sessions
                 WHERE tenant = $1 AND (last_activity, session_id) < ($2, $3)
                   AND (($5 = 'all')
                     OR ($5 = 'roots' AND parent_session IS NULL)
                     OR ($5 = 'children' AND parent_session = $6))
                 ORDER BY last_activity DESC, session_id DESC LIMIT $4",
            )
            .bind(tenant)
            .bind(at)
            .bind(id)
            .bind(limit as i64)
            .bind(scope_kind)
            .bind(scope_parent),
            None => sqlx::query(
                "SELECT session_id, label, event_count, created_at, last_activity,
                        agent, parent_session,
                        run_count, errored_runs, input_tokens, output_tokens,
                        last_run_status, last_run_at
                 FROM runic.sessions WHERE tenant = $1
                   AND (($3 = 'all')
                     OR ($3 = 'roots' AND parent_session IS NULL)
                     OR ($3 = 'children' AND parent_session = $4))
                 ORDER BY last_activity DESC, session_id DESC LIMIT $2",
            )
            .bind(tenant)
            .bind(limit as i64)
            .bind(scope_kind)
            .bind(scope_parent),
        }
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.into_iter().map(row_to_meta).collect()
    }

    async fn create_child_session(
        &self,
        tenant: &str,
        session_id: &str,
        parent_session: &str,
        agent: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let parent: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM runic.sessions WHERE tenant = $1 AND session_id = $2 FOR SHARE",
        )
        .bind(tenant)
        .bind(parent_session)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        if parent.is_none() {
            return Err(Error::NotFound(format!("parent session {parent_session}")));
        }
        sqlx::query(
            "INSERT INTO runic.sessions (tenant, session_id, parent_session, agent)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (tenant, session_id) DO UPDATE
               SET parent_session = EXCLUDED.parent_session,
                   agent = EXCLUDED.agent",
        )
        .bind(tenant)
        .bind(session_id)
        .bind(parent_session)
        .bind(agent)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn list_sessions(&self, tenant: &str) -> Result<Vec<SessionMeta>> {
        let rows = sqlx::query(
            "SELECT session_id, label, event_count, created_at, last_activity,
                        agent, parent_session,
                        run_count, errored_runs, input_tokens, output_tokens,
                        last_run_status, last_run_at
             FROM runic.sessions WHERE tenant = $1 ORDER BY last_activity DESC",
        )
        .bind(tenant)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(row_to_meta(row)?);
        }
        Ok(out)
    }

    async fn session_meta(&self, tenant: &str, session_id: &str) -> Result<Option<SessionMeta>> {
        let row = sqlx::query(
            "SELECT session_id, label, event_count, created_at, last_activity,
                        agent, parent_session,
                        run_count, errored_runs, input_tokens, output_tokens,
                        last_run_status, last_run_at
             FROM runic.sessions WHERE tenant = $1 AND session_id = $2",
        )
        .bind(tenant)
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;

        row.map(row_to_meta).transpose()
    }

    async fn set_label(&self, tenant: &str, session_id: &str, label: Option<&str>) -> Result<()> {
        sqlx::query(
            "INSERT INTO runic.sessions (tenant, session_id, label)
             VALUES ($1, $2, $3)
             ON CONFLICT (tenant, session_id) DO UPDATE
               SET label = EXCLUDED.label",
        )
        .bind(tenant)
        .bind(session_id)
        .bind(label)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn delete_session(&self, tenant: &str, session_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM runic.sessions WHERE tenant = $1 AND session_id = $2")
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
             FROM runic.chats, websearch_to_tsquery('english', $2) q
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
        let res = sqlx::query("DELETE FROM runic.sessions WHERE last_activity < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(res.rows_affected())
    }

    async fn list_tenants(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT DISTINCT tenant FROM runic.sessions ORDER BY tenant")
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.into_iter()
            .map(|r| r.try_get::<String, _>("tenant").map_err(db))
            .collect()
    }
}
