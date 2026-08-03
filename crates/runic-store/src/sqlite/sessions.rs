use std::path::Path;
use std::str::FromStr;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};

use crate::SessionEvent;
use runic_types::Role;

use super::{db, migrate, serde};
use crate::sessions::{SessionScope, event_at};
use crate::{ChatHit, Error, Result, SessionMeta, SessionStore, StoredEvent};

const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub struct SqliteSessionStore {
    pool: SqlitePool,
}

impl SqliteSessionStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path.as_ref())
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(BUSY_TIMEOUT)
            .foreign_keys(true);
        Self::with_options(options).await
    }

    pub async fn memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(db)?
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(db)?;
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    async fn with_options(options: SqliteConnectOptions) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .map_err(db)?;
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

fn event_kind(event: &SessionEvent) -> &'static str {
    match event {
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

const META_COLUMNS: &str = "session_id, label, event_count, created_at, last_activity, agent, \
                            parent_session, run_count, errored_runs, input_tokens, \
                            output_tokens, last_run_status, last_run_at";

fn rows_to_events(rows: Vec<SqliteRow>) -> Result<Vec<StoredEvent>> {
    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        let seq: i64 = row.try_get("seq").map_err(db)?;
        let raw: String = row.try_get("event").map_err(db)?;
        events.push(StoredEvent {
            seq: seq as u64,
            event: serde_json::from_str(&raw).map_err(serde)?,
        });
    }
    Ok(events)
}

fn row_to_meta(row: SqliteRow) -> Result<SessionMeta> {
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

async fn write_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    tenant: &str,
    session_id: &str,
    event: &SessionEvent,
) -> Result<i64> {
    let at = event_at(event);
    let kind = event_kind(event);
    let run_id = event.run_id().to_string();
    let json = serde_json::to_string(event).map_err(serde)?;
    let delta = crate::sessions::summary_delta(event);

    let seq: i64 = sqlx::query_scalar(
        "INSERT INTO sessions (tenant, session_id, last_seq, event_count, last_activity,
                               run_count, errored_runs, input_tokens, output_tokens,
                               last_run_status, last_run_at)
         VALUES (?1, ?2, 1, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (tenant, session_id) DO UPDATE
           SET last_seq = sessions.last_seq + 1,
               event_count = sessions.event_count + 1,
               last_activity = excluded.last_activity,
               run_count = sessions.run_count + excluded.run_count,
               errored_runs = sessions.errored_runs + excluded.errored_runs,
               input_tokens = sessions.input_tokens + excluded.input_tokens,
               output_tokens = sessions.output_tokens + excluded.output_tokens,
               last_run_status = COALESCE(excluded.last_run_status, sessions.last_run_status),
               last_run_at = COALESCE(excluded.last_run_at, sessions.last_run_at)
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
        "INSERT INTO events (tenant, session_id, seq, kind, run_id, at, event)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
                    "INSERT INTO chats (tenant, session_id, seq, role, text, at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
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

fn fts_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|token| token.replace('"', ""))
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
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
        let exists: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM sessions WHERE tenant = ?1 AND session_id = ?2")
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
            "SELECT seq, event FROM events
             WHERE tenant = ?1 AND session_id = ?2 ORDER BY seq",
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
            "SELECT seq, event FROM events
             WHERE tenant = ?1 AND session_id = ?2
               AND seq >= COALESCE((
                 SELECT MAX(seq) FROM events
                 WHERE tenant = ?1 AND session_id = ?2 AND kind = 'StateSnapshot'
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
            "SELECT seq, event FROM events
             WHERE tenant = ?1 AND session_id = ?2 AND seq > ?3 ORDER BY seq",
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
            "SELECT seq, event FROM events
             WHERE tenant = ?1 AND session_id = ?2 AND run_id = ?3 AND seq > ?4 ORDER BY seq",
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
            "SELECT seq, event FROM events
             WHERE tenant = ?1 AND session_id = ?2 AND seq > ?3 ORDER BY seq LIMIT ?4",
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

    async fn list_sessions(&self, tenant: &str) -> Result<Vec<SessionMeta>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {META_COLUMNS} FROM sessions WHERE tenant = ?1 ORDER BY last_activity DESC"
        )))
        .bind(tenant)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.into_iter().map(row_to_meta).collect()
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
            Some((at, id)) => sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT {META_COLUMNS} FROM sessions
                 WHERE tenant = ?1 AND (last_activity, session_id) < (?2, ?3)
                   AND ((?5 = 'all')
                     OR (?5 = 'roots' AND parent_session IS NULL)
                     OR (?5 = 'children' AND parent_session = ?6))
                 ORDER BY last_activity DESC, session_id DESC LIMIT ?4"
            )))
            .bind(tenant)
            .bind(at)
            .bind(id)
            .bind(limit as i64)
            .bind(scope_kind)
            .bind(scope_parent),
            None => sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT {META_COLUMNS} FROM sessions
                 WHERE tenant = ?1
                   AND ((?3 = 'all')
                     OR (?3 = 'roots' AND parent_session IS NULL)
                     OR (?3 = 'children' AND parent_session = ?4))
                 ORDER BY last_activity DESC, session_id DESC LIMIT ?2"
            )))
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
        let parent: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM sessions WHERE tenant = ?1 AND session_id = ?2")
                .bind(tenant)
                .bind(parent_session)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?;
        if parent.is_none() {
            return Err(Error::NotFound(format!("parent session {parent_session}")));
        }
        sqlx::query(
            "INSERT INTO sessions (tenant, session_id, parent_session, agent)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (tenant, session_id) DO UPDATE
               SET parent_session = excluded.parent_session,
                   agent = excluded.agent",
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

    async fn session_meta(&self, tenant: &str, session_id: &str) -> Result<Option<SessionMeta>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {META_COLUMNS} FROM sessions WHERE tenant = ?1 AND session_id = ?2"
        )))
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
             VALUES (?1, ?2, ?3)
             ON CONFLICT (tenant, session_id) DO UPDATE SET label = excluded.label",
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
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("DELETE FROM chats WHERE tenant = ?1 AND session_id = ?2")
            .bind(tenant)
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM events WHERE tenant = ?1 AND session_id = ?2")
            .bind(tenant)
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("DELETE FROM sessions WHERE tenant = ?1 AND session_id = ?2")
            .bind(tenant)
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(())
    }

    async fn search(
        &self,
        tenant: &str,
        query: &str,
        limit: usize,
        exclude_session: Option<&str>,
    ) -> Result<Vec<ChatHit>> {
        let terms = fts_query(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT session_id, seq, role, at,
                    snippet(chats, 4, '', '', '…', 32) AS snippet
             FROM chats
             WHERE chats MATCH ?2 AND tenant = ?1
               AND (?3 IS NULL OR session_id <> ?3)
             ORDER BY rank
             LIMIT ?4",
        )
        .bind(tenant)
        .bind(terms)
        .bind(exclude_session)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;

        let mut hits = Vec::with_capacity(rows.len());
        for row in rows {
            hits.push(ChatHit {
                session_id: row.try_get("session_id").map_err(db)?,
                seq: row.try_get::<i64, _>("seq").map_err(db)? as u64,
                role: row.try_get("role").map_err(db)?,
                snippet: row.try_get("snippet").map_err(db)?,
                at: row.try_get("at").map_err(db)?,
            });
        }
        Ok(hits)
    }

    async fn cleanup_stale(&self, ttl: Duration) -> Result<u64> {
        let cutoff = Utc::now() - ttl;
        let stale: Vec<(String, String)> =
            sqlx::query("SELECT tenant, session_id FROM sessions WHERE last_activity < ?1")
                .bind(cutoff)
                .fetch_all(&self.pool)
                .await
                .map_err(db)?
                .into_iter()
                .map(|row| {
                    Ok::<_, Error>((
                        row.try_get("tenant").map_err(db)?,
                        row.try_get("session_id").map_err(db)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;

        for (tenant, session_id) in &stale {
            self.delete_session(tenant, session_id).await?;
        }
        Ok(stale.len() as u64)
    }

    async fn list_tenants(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT DISTINCT tenant FROM sessions ORDER BY tenant")
            .fetch_all(&self.pool)
            .await
            .map_err(db)?;
        rows.into_iter()
            .map(|row| row.try_get::<String, _>("tenant").map_err(db))
            .collect()
    }
}
