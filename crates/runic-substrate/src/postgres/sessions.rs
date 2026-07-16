//! Postgres-backed [`SessionStore`].
//!
//! Event log in `session_events` (append-only, `(tenant, session_id, seq)`),
//! per-session counter + metadata in `sessions`, and a full-text projection of
//! conversational messages in `chat_messages` (for `search`).

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sqlx::{PgPool, Row};

use runic_state::SessionEvent;
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

const RUN_COLUMNS: &str = "run_id, tenant, session_id, agent, status, error, claimed_by, \
     lease_expires_at, input, context, cancel_requested, created_at, updated_at";

fn row_to_run(row: sqlx::postgres::PgRow) -> Result<crate::RunRecord> {
    let status: String = row.try_get("status").map_err(db)?;
    Ok(crate::RunRecord {
        run_id: row.try_get("run_id").map_err(db)?,
        tenant: row.try_get("tenant").map_err(db)?,
        session_id: row.try_get("session_id").map_err(db)?,
        agent: row.try_get("agent").map_err(db)?,
        status: crate::RunStatus::parse(&status)
            .ok_or_else(|| crate::Error::Serde(format!("unknown run status {status:?}")))?,
        error: row.try_get("error").map_err(db)?,
        claimed_by: row.try_get("claimed_by").map_err(db)?,
        lease_expires_at: row.try_get("lease_expires_at").map_err(db)?,
        input: row.try_get("input").map_err(db)?,
        context: row.try_get("context").map_err(db)?,
        cancel_requested: row.try_get("cancel_requested").map_err(db)?,
        created_at: row.try_get("created_at").map_err(db)?,
        updated_at: row.try_get("updated_at").map_err(db)?,
    })
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
        "INSERT INTO sessions (tenant, session_id, last_seq, event_count, last_activity,
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
            "SELECT 1 FROM sessions WHERE tenant = $1 AND session_id = $2 FOR UPDATE",
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

    async fn read_tail(&self, tenant: &str, session_id: &str) -> Result<Vec<StoredEvent>> {
        let rows = sqlx::query(
            "SELECT seq, event FROM session_events
             WHERE tenant = $1 AND session_id = $2
               AND seq >= COALESCE((
                 SELECT MAX(seq) FROM session_events
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

    async fn read_after_limited(
        &self,
        tenant: &str,
        session_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        let rows = sqlx::query(
            "SELECT seq, event FROM session_events
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
                 FROM sessions
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
                 FROM sessions WHERE tenant = $1
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
        sqlx::query(
            "INSERT INTO sessions (tenant, session_id, parent_session, agent)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (tenant, session_id) DO UPDATE
               SET parent_session = EXCLUDED.parent_session,
                   agent = EXCLUDED.agent",
        )
        .bind(tenant)
        .bind(session_id)
        .bind(parent_session)
        .bind(agent)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn list_sessions(&self, tenant: &str) -> Result<Vec<SessionMeta>> {
        let rows = sqlx::query(
            "SELECT session_id, label, event_count, created_at, last_activity,
                        agent, parent_session,
                        run_count, errored_runs, input_tokens, output_tokens,
                        last_run_status, last_run_at
             FROM sessions WHERE tenant = $1 ORDER BY last_activity DESC",
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
             FROM sessions WHERE tenant = $1 AND session_id = $2",
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
            "INSERT INTO sessions (tenant, session_id, label)
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
        sqlx::query("DELETE FROM sessions WHERE tenant = $1 AND session_id = $2")
            .bind(tenant)
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM runs WHERE tenant = $1 AND session_id = $2")
            .bind(tenant)
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM thread_leases WHERE tenant = $1 AND session_id = $2")
            .bind(tenant)
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn create_run(
        &self,
        tenant: &str,
        session_id: &str,
        run_id: &str,
        agent: &str,
        input: &crate::RunInput,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO runs (run_id, tenant, session_id, agent, status, input, context)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(run_id)
        .bind(tenant)
        .bind(session_id)
        .bind(agent)
        .bind(if input.queued { "queued" } else { "pending" })
        .bind(&input.input)
        .bind(&input.context)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn set_run_status(
        &self,
        run_id: &str,
        status: crate::RunStatus,
        error: Option<&str>,
    ) -> Result<()> {
        let result = sqlx::query(
            "UPDATE runs SET status = $2, error = $3, updated_at = now() WHERE run_id = $1",
        )
        .bind(run_id)
        .bind(status.as_str())
        .bind(error)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        if result.rows_affected() == 0 {
            return Err(crate::Error::NotFound(format!("run {run_id}")));
        }
        Ok(())
    }

    async fn claim_run(
        &self,
        run_id: &str,
        claimed_by: &str,
        lease: chrono::Duration,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE runs
             SET status = 'running', claimed_by = $2,
                 lease_expires_at = now() + make_interval(secs => $3),
                 updated_at = now()
             WHERE run_id = $1 AND status IN ('pending', 'queued') AND claimed_by IS NULL",
        )
        .bind(run_id)
        .bind(claimed_by)
        .bind(lease.num_milliseconds() as f64 / 1000.0)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(result.rows_affected() > 0)
    }

    async fn heartbeat_run(
        &self,
        run_id: &str,
        claimed_by: &str,
        lease: chrono::Duration,
    ) -> Result<Option<crate::RunSignals>> {
        let row = sqlx::query(
            "UPDATE runs r
             SET lease_expires_at = now() + make_interval(secs => $3),
                 steering = NULL, updated_at = now()
             FROM (SELECT run_id, steering FROM runs WHERE run_id = $1 FOR UPDATE) old
             WHERE r.run_id = old.run_id AND r.claimed_by = $2 AND r.status = 'running'
             RETURNING r.cancel_requested, old.steering",
        )
        .bind(run_id)
        .bind(claimed_by)
        .bind(lease.num_milliseconds() as f64 / 1000.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let cancel_requested: bool = row.try_get("cancel_requested").map_err(db)?;
        let steering: Option<serde_json::Value> = row.try_get("steering").map_err(db)?;
        let steering = steering
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default();
        Ok(Some(crate::RunSignals {
            cancel_requested,
            steering,
        }))
    }

    async fn request_cancel_run(&self, tenant: &str, run_id: &str) -> Result<bool> {
        let dropped = sqlx::query(
            "UPDATE runs SET status = 'cancelled', updated_at = now()
             WHERE run_id = $1 AND tenant = $2
               AND (status = 'paused' OR (status = 'queued' AND claimed_by IS NULL))",
        )
        .bind(run_id)
        .bind(tenant)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        if dropped.rows_affected() > 0 {
            return Ok(true);
        }
        let flagged = sqlx::query(
            "UPDATE runs SET cancel_requested = TRUE, updated_at = now()
             WHERE run_id = $1 AND tenant = $2
               AND status IN ('pending', 'queued', 'running', 'paused')",
        )
        .bind(run_id)
        .bind(tenant)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(flagged.rows_affected() > 0)
    }

    async fn push_steering(&self, tenant: &str, run_id: &str, text: &str) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE runs
             SET steering = COALESCE(steering, '[]'::jsonb) || to_jsonb($3::text),
                 updated_at = now()
             WHERE run_id = $1 AND tenant = $2
               AND status IN ('pending', 'queued', 'running', 'paused')",
        )
        .bind(run_id)
        .bind(tenant)
        .bind(text)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(result.rows_affected() > 0)
    }

    async fn claim_thread(
        &self,
        tenant: &str,
        session_id: &str,
        claimed_by: &str,
        lease: chrono::Duration,
    ) -> Result<bool> {
        let result = sqlx::query(
            "INSERT INTO thread_leases (tenant, session_id, claimed_by, lease_expires_at)
             VALUES ($1, $2, $3, now() + make_interval(secs => $4))
             ON CONFLICT (tenant, session_id) DO UPDATE
               SET claimed_by = EXCLUDED.claimed_by,
                   lease_expires_at = EXCLUDED.lease_expires_at
               WHERE thread_leases.lease_expires_at < now()
                  OR thread_leases.claimed_by = EXCLUDED.claimed_by",
        )
        .bind(tenant)
        .bind(session_id)
        .bind(claimed_by)
        .bind(lease.num_milliseconds() as f64 / 1000.0)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(result.rows_affected() > 0)
    }

    async fn extend_thread_lease(
        &self,
        tenant: &str,
        session_id: &str,
        claimed_by: &str,
        lease: chrono::Duration,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE thread_leases
             SET lease_expires_at = now() + make_interval(secs => $4)
             WHERE tenant = $1 AND session_id = $2 AND claimed_by = $3",
        )
        .bind(tenant)
        .bind(session_id)
        .bind(claimed_by)
        .bind(lease.num_milliseconds() as f64 / 1000.0)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(result.rows_affected() > 0)
    }

    async fn release_thread(&self, tenant: &str, session_id: &str, claimed_by: &str) -> Result<()> {
        sqlx::query(
            "DELETE FROM thread_leases
             WHERE tenant = $1 AND session_id = $2 AND claimed_by = $3",
        )
        .bind(tenant)
        .bind(session_id)
        .bind(claimed_by)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn reap_expired_runs(&self) -> Result<Vec<crate::RunRecord>> {
        let rows = sqlx::query(&format!(
            "UPDATE runs
             SET status = 'error', error = 'lease expired', updated_at = now()
             WHERE status = 'running' AND lease_expires_at < now()
             RETURNING {RUN_COLUMNS}"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        let reaped: Vec<crate::RunRecord> =
            rows.into_iter().map(row_to_run).collect::<Result<_>>()?;
        for run in &reaped {
            sqlx::query(
                "UPDATE sessions SET last_run_status = 'failed'
                 WHERE tenant = $1 AND session_id = $2
                   AND last_run_status = 'running' AND last_run_at <= $3",
            )
            .bind(&run.tenant)
            .bind(&run.session_id)
            .bind(run.created_at)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        }
        Ok(reaped)
    }

    async fn claim_next_queued_run(
        &self,
        claimed_by: &str,
        lease: chrono::Duration,
    ) -> Result<Option<crate::RunRecord>> {
        let row = sqlx::query(&format!(
            "UPDATE runs
             SET status = 'running', claimed_by = $1,
                 lease_expires_at = now() + make_interval(secs => $2),
                 updated_at = now()
             WHERE run_id = (
                 SELECT run_id FROM runs
                 WHERE status = 'queued' AND claimed_by IS NULL
                 ORDER BY created_at
                 LIMIT 1
                 FOR UPDATE SKIP LOCKED
             )
             RETURNING {RUN_COLUMNS}"
        ))
        .bind(claimed_by)
        .bind(lease.num_milliseconds() as f64 / 1000.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        row.map(row_to_run).transpose()
    }

    async fn release_run(&self, run_id: &str, claimed_by: &str) -> Result<()> {
        sqlx::query(
            "UPDATE runs
             SET status = 'queued', claimed_by = NULL, lease_expires_at = NULL,
                 updated_at = now()
             WHERE run_id = $1 AND claimed_by = $2",
        )
        .bind(run_id)
        .bind(claimed_by)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    async fn resume_run(&self, tenant: &str, run_id: &str) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE runs
             SET status = 'queued', claimed_by = NULL, lease_expires_at = NULL,
                 updated_at = now()
             WHERE run_id = $1 AND tenant = $2 AND status = 'paused'",
        )
        .bind(run_id)
        .bind(tenant)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(result.rows_affected() > 0)
    }

    async fn deliver_and_resume(
        &self,
        tenant: &str,
        run_id: &str,
        event: &SessionEvent,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let row = sqlx::query(
            "UPDATE runs
             SET status = 'queued', claimed_by = NULL, lease_expires_at = NULL,
                 updated_at = now()
             WHERE run_id = $1 AND tenant = $2 AND status = 'paused'
             RETURNING session_id",
        )
        .bind(run_id)
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        let Some(row) = row else {
            tx.rollback().await.map_err(db)?;
            return Ok(false);
        };
        let session_id: String = row.try_get("session_id").map_err(db)?;
        write_event(&mut tx, tenant, &session_id, event).await?;
        tx.commit().await.map_err(db)?;
        Ok(true)
    }

    async fn get_run(&self, tenant: &str, run_id: &str) -> Result<Option<crate::RunRecord>> {
        let row = sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE tenant = $1 AND run_id = $2"
        ))
        .bind(tenant)
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        row.map(row_to_run).transpose()
    }

    async fn list_runs(
        &self,
        tenant: &str,
        session_id: &str,
        limit: usize,
        before: Option<(chrono::DateTime<chrono::Utc>, String)>,
    ) -> Result<Vec<crate::RunRecord>> {
        let rows = match before {
            Some((cut_at, cut_id)) => {
                sqlx::query(&format!(
                    "SELECT {RUN_COLUMNS} FROM runs \
                     WHERE tenant = $1 AND session_id = $2 AND (created_at, run_id) < ($3, $4) \
                     ORDER BY created_at DESC, run_id DESC LIMIT $5"
                ))
                .bind(tenant)
                .bind(session_id)
                .bind(cut_at)
                .bind(cut_id)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(&format!(
                    "SELECT {RUN_COLUMNS} FROM runs \
                     WHERE tenant = $1 AND session_id = $2 \
                     ORDER BY created_at DESC, run_id DESC LIMIT $3"
                ))
                .bind(tenant)
                .bind(session_id)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(db)?;
        rows.into_iter().map(row_to_run).collect()
    }

    async fn latest_active_run(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Option<crate::RunRecord>> {
        let row = sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM runs
             WHERE tenant = $1 AND session_id = $2
               AND status IN ('pending', 'queued', 'running', 'paused')
             ORDER BY (status = 'running') DESC, created_at DESC
             LIMIT 1"
        ))
        .bind(tenant)
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        row.map(row_to_run).transpose()
    }

    async fn latest_run(&self, tenant: &str, session_id: &str) -> Result<Option<crate::RunRecord>> {
        let row = sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE tenant = $1 AND session_id = $2
             ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(tenant)
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        row.map(row_to_run).transpose()
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
