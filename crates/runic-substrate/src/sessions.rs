//! The event-sourced [`SessionStore`] trait + its value types.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use runic_state::SessionEvent;

use crate::{Error, Result};

/// Timestamp of any [`SessionEvent`] variant.
pub(crate) fn event_at(e: &SessionEvent) -> DateTime<Utc> {
    match e {
        SessionEvent::RunStart { at, .. }
        | SessionEvent::RunEnd { at, .. }
        | SessionEvent::Message { at, .. }
        | SessionEvent::TurnEnd { at, .. }
        | SessionEvent::ToolStarted { at, .. }
        | SessionEvent::ToolFinished { at, .. }
        | SessionEvent::DelegationStarted { at, .. }
        | SessionEvent::DelegationFinished { at, .. }
        | SessionEvent::HookFired { at, .. }
        | SessionEvent::StateSnapshot { at, .. }
        | SessionEvent::TaskSpawned { at, .. }
        | SessionEvent::TaskFinished { at, .. }
        | SessionEvent::StateUpdated { at, .. }
        | SessionEvent::ToolDeferred { at, .. } => *at,
    }
}

/// An event as stored, with its assigned monotonic sequence number.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    /// Store-assigned, strictly increasing within `(tenant, session_id)`.
    pub seq: u64,
    pub event: SessionEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Queued,
    Running,
    Paused,
    Success,
    Error,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Pending => "pending",
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::Paused => "paused",
            RunStatus::Success => "success",
            RunStatus::Error => "error",
            RunStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<RunStatus> {
        match s {
            "pending" => Some(RunStatus::Pending),
            "queued" => Some(RunStatus::Queued),
            "running" => Some(RunStatus::Running),
            "paused" => Some(RunStatus::Paused),
            "success" => Some(RunStatus::Success),
            "error" => Some(RunStatus::Error),
            "cancelled" => Some(RunStatus::Cancelled),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Success | RunStatus::Error | RunStatus::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub tenant: String,
    pub session_id: String,
    pub agent: String,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
    #[serde(default)]
    pub cancel_requested: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct RunSignals {
    pub cancel_requested: bool,
    pub steering: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RunInput {
    pub input: Option<serde_json::Value>,
    pub context: Option<serde_json::Value>,
    pub queued: bool,
}

/// Per-session metadata, for listing without scanning the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub label: Option<String>,
    pub event_count: u64,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(default)]
    pub run_count: u64,
    #[serde(default)]
    pub errored_runs: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SummaryDelta {
    pub runs: u64,
    pub errored: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub last_run_status: Option<&'static str>,
    pub last_run_at: Option<DateTime<Utc>>,
}

pub(crate) fn summary_delta(event: &SessionEvent) -> SummaryDelta {
    match event {
        SessionEvent::RunStart { at, .. } => SummaryDelta {
            runs: 1,
            last_run_status: Some("running"),
            last_run_at: Some(*at),
            ..Default::default()
        },
        SessionEvent::RunEnd { status, .. } => SummaryDelta {
            errored: matches!(status, runic_state::RunEndStatus::Failed(_)) as u64,
            last_run_status: Some(match status {
                runic_state::RunEndStatus::Completed => "completed",
                runic_state::RunEndStatus::Failed(_) => "failed",
                runic_state::RunEndStatus::Cancelled => "cancelled",
            }),
            ..Default::default()
        },
        SessionEvent::TurnEnd { usage, .. } => SummaryDelta {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            ..Default::default()
        },
        _ => SummaryDelta::default(),
    }
}

/// Which sessions a listing covers: top-level threads (the default), the
/// children of one parent, or everything.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SessionScope {
    #[default]
    Roots,
    ChildrenOf(String),
    All,
}

impl SessionScope {
    pub fn matches(&self, meta: &SessionMeta) -> bool {
        match self {
            SessionScope::Roots => meta.parent_session.is_none(),
            SessionScope::ChildrenOf(parent) => meta.parent_session.as_deref() == Some(parent),
            SessionScope::All => true,
        }
    }
}

/// A textual-search hit from [`SessionStore::search`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatHit {
    pub session_id: String,
    pub seq: u64,
    pub role: String,
    /// Highlighted snippet around the match.
    pub snippet: String,
    pub at: DateTime<Utc>,
}

/// Pluggable, multi-tenant, event-sourced session persistence.
///
/// Every method is scoped by `tenant` first — `list_sessions("alice")` never
/// returns Bob's sessions. Pass `"default"` for single-user deployments.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Append one event; the store assigns and returns its `seq`.
    async fn append(&self, tenant: &str, session_id: &str, event: &SessionEvent) -> Result<u64>;

    async fn append_batch(
        &self,
        tenant: &str,
        session_id: &str,
        events: &[SessionEvent],
    ) -> Result<()>;

    /// Like [`append_batch`](Self::append_batch), but fails with
    /// [`Error::NotFound`] instead of materializing the session row. Detached
    /// writers (child transcript persisters) use this so a late batch can
    /// never resurrect a deleted session. Default is check-then-append;
    /// override to close the race atomically.
    async fn append_batch_strict(
        &self,
        tenant: &str,
        session_id: &str,
        events: &[SessionEvent],
    ) -> Result<()> {
        if self.session_meta(tenant, session_id).await?.is_none() {
            return Err(Error::NotFound(format!("session {session_id}")));
        }
        self.append_batch(tenant, session_id, events).await
    }

    /// Read every event for a session, in `seq` order.
    async fn read(&self, tenant: &str, session_id: &str) -> Result<Vec<StoredEvent>>;

    async fn read_tail(&self, tenant: &str, session_id: &str) -> Result<Vec<StoredEvent>> {
        let mut all = self.read(tenant, session_id).await?;
        if let Some(i) = all
            .iter()
            .rposition(|s| matches!(s.event, SessionEvent::StateSnapshot { .. }))
        {
            all.drain(..i);
        }
        Ok(all)
    }

    /// Read events with `seq > after_seq` — for tailing (poll with the last
    /// seen seq).
    async fn read_after(
        &self,
        tenant: &str,
        session_id: &str,
        after_seq: u64,
    ) -> Result<Vec<StoredEvent>>;

    /// Read one run's events with `seq > after_seq`. Default filters
    /// `read_after` in memory; override to push the filter into the store.
    async fn read_run_after(
        &self,
        tenant: &str,
        session_id: &str,
        run_id: &str,
        after_seq: u64,
    ) -> Result<Vec<StoredEvent>> {
        let all = self.read_after(tenant, session_id, after_seq).await?;
        Ok(all
            .into_iter()
            .filter(|s| s.event.run_id() == run_id)
            .collect())
    }

    /// Read up to `limit` events with `seq > after_seq`, in seq order. Default
    /// truncates `read_after`; override to push the LIMIT into the store.
    async fn read_after_limited(
        &self,
        tenant: &str,
        session_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        let mut events = self.read_after(tenant, session_id, after_seq).await?;
        events.truncate(limit);
        Ok(events)
    }

    /// List a tenant's sessions with metadata, most-recently-active first.
    async fn list_sessions(&self, tenant: &str) -> Result<Vec<SessionMeta>>;

    /// A page of `list_sessions` after the `(last_activity, session_id)` keyset
    /// cursor, restricted to `scope`. Default filters `list_sessions`; override
    /// to push the keyset + scope + LIMIT into the store.
    async fn list_sessions_page(
        &self,
        tenant: &str,
        after: Option<(DateTime<Utc>, String)>,
        limit: usize,
        scope: SessionScope,
    ) -> Result<Vec<SessionMeta>> {
        let all = self.list_sessions(tenant).await?;
        Ok(all
            .into_iter()
            .filter(|m| scope.matches(m))
            .filter(|m| match &after {
                Some((at, id)) => (m.last_activity, m.session_id.as_str()) < (*at, id.as_str()),
                None => true,
            })
            .take(limit)
            .collect())
    }

    /// Materialize a child session owned by `parent_session`, run by `agent`.
    /// Fails with [`Error::NotFound`] when the parent row doesn't exist, so a
    /// child can never be created under a deleted (or never-created) parent.
    async fn create_child_session(
        &self,
        _tenant: &str,
        _session_id: &str,
        _parent_session: &str,
        _agent: &str,
    ) -> Result<()> {
        Err(Error::Unsupported("child sessions".to_string()))
    }

    /// Delete child sessions whose parent row no longer exists, repeating
    /// until none remain (reaping an orphan may orphan its own children).
    /// Returns the deleted ids so callers can clean up their artifacts.
    async fn delete_orphan_children(&self, tenant: &str) -> Result<Vec<String>> {
        let mut reaped = Vec::new();
        loop {
            let all = self.list_sessions(tenant).await?;
            let ids: std::collections::HashSet<&str> =
                all.iter().map(|m| m.session_id.as_str()).collect();
            let orphans: Vec<String> = all
                .iter()
                .filter(|m| {
                    m.parent_session
                        .as_deref()
                        .is_some_and(|p| !ids.contains(p))
                })
                .map(|m| m.session_id.clone())
                .collect();
            if orphans.is_empty() {
                return Ok(reaped);
            }
            for orphan in orphans {
                self.delete_session(tenant, &orphan).await?;
                reaped.push(orphan);
            }
        }
    }

    /// Read one session's metadata without scanning the event log.
    async fn session_meta(&self, tenant: &str, session_id: &str) -> Result<Option<SessionMeta>>;

    /// Set the durable session label. Implementations should upsert session
    /// metadata so titled empty sessions are materialized.
    async fn set_label(&self, tenant: &str, session_id: &str, label: Option<&str>) -> Result<()>;

    /// Delete a session and all its events.
    async fn delete_session(&self, tenant: &str, session_id: &str) -> Result<()>;

    async fn create_run(
        &self,
        _tenant: &str,
        _session_id: &str,
        _run_id: &str,
        _agent: &str,
        _input: &RunInput,
    ) -> Result<()> {
        Err(Error::Unsupported("create_run".into()))
    }

    async fn list_runs(
        &self,
        _tenant: &str,
        _session_id: &str,
        _limit: usize,
        _before: Option<(DateTime<Utc>, String)>,
    ) -> Result<Vec<RunRecord>> {
        Err(crate::Error::Database(
            "list_runs not supported by this backend".into(),
        ))
    }

    async fn set_run_status(
        &self,
        _run_id: &str,
        _status: RunStatus,
        _error: Option<&str>,
    ) -> Result<()> {
        Err(Error::Unsupported("set_run_status".into()))
    }

    async fn claim_run(
        &self,
        _run_id: &str,
        _claimed_by: &str,
        _lease: chrono::Duration,
    ) -> Result<bool> {
        Err(Error::Unsupported("claim_run".into()))
    }

    async fn heartbeat_run(
        &self,
        _run_id: &str,
        _claimed_by: &str,
        _lease: chrono::Duration,
    ) -> Result<Option<RunSignals>> {
        Err(Error::Unsupported("heartbeat_run".into()))
    }

    async fn request_cancel_run(&self, _tenant: &str, _run_id: &str) -> Result<bool> {
        Err(Error::Unsupported("request_cancel_run".into()))
    }

    async fn push_steering(&self, _tenant: &str, _run_id: &str, _text: &str) -> Result<bool> {
        Err(Error::Unsupported("push_steering".into()))
    }

    async fn claim_thread(
        &self,
        _tenant: &str,
        _session_id: &str,
        _claimed_by: &str,
        _lease: chrono::Duration,
    ) -> Result<bool> {
        Err(Error::Unsupported("claim_thread".into()))
    }

    async fn extend_thread_lease(
        &self,
        _tenant: &str,
        _session_id: &str,
        _claimed_by: &str,
        _lease: chrono::Duration,
    ) -> Result<bool> {
        Err(Error::Unsupported("extend_thread_lease".into()))
    }

    async fn release_thread(
        &self,
        _tenant: &str,
        _session_id: &str,
        _claimed_by: &str,
    ) -> Result<()> {
        Err(Error::Unsupported("release_thread".into()))
    }

    async fn reap_expired_runs(&self) -> Result<Vec<RunRecord>> {
        Err(Error::Unsupported("reap_expired_runs".into()))
    }

    async fn claim_next_queued_run(
        &self,
        _claimed_by: &str,
        _lease: chrono::Duration,
    ) -> Result<Option<RunRecord>> {
        Err(Error::Unsupported("claim_next_queued_run".into()))
    }

    async fn release_run(&self, _run_id: &str, _claimed_by: &str) -> Result<()> {
        Err(Error::Unsupported("release_run".into()))
    }

    async fn resume_run(&self, _tenant: &str, _run_id: &str) -> Result<bool> {
        Err(Error::Unsupported("resume_run".into()))
    }

    async fn deliver_and_resume(
        &self,
        tenant: &str,
        run_id: &str,
        event: &SessionEvent,
    ) -> Result<bool> {
        let Some(rec) = self.get_run(tenant, run_id).await? else {
            return Ok(false);
        };
        if rec.status != crate::RunStatus::Paused {
            return Ok(false);
        }
        self.append(tenant, &rec.session_id, event).await?;
        self.resume_run(tenant, run_id).await
    }

    async fn get_run(&self, _tenant: &str, _run_id: &str) -> Result<Option<RunRecord>> {
        Err(Error::Unsupported("get_run".into()))
    }

    async fn latest_run(&self, _tenant: &str, _session_id: &str) -> Result<Option<RunRecord>> {
        Err(Error::Unsupported("latest_run".into()))
    }

    async fn latest_active_run(
        &self,
        _tenant: &str,
        _session_id: &str,
    ) -> Result<Option<RunRecord>> {
        Err(Error::Unsupported("latest_active_run".into()))
    }

    /// Textual (NOT semantic) full-text search over a tenant's conversations.
    /// Default: unsupported.
    async fn search(
        &self,
        _tenant: &str,
        _query: &str,
        _limit: usize,
        _exclude_session: Option<&str>,
    ) -> Result<Vec<ChatHit>> {
        Err(Error::Unsupported("search".into()))
    }

    /// Delete sessions whose last activity is older than `ttl`; returns the
    /// number deleted. Default: unsupported.
    async fn cleanup_stale(&self, _ttl: chrono::Duration) -> Result<u64> {
        Err(Error::Unsupported("cleanup_stale".into()))
    }

    /// All tenants known to the store. Default: unsupported.
    async fn list_tenants(&self) -> Result<Vec<String>> {
        Err(Error::Unsupported("list_tenants".into()))
    }
}
