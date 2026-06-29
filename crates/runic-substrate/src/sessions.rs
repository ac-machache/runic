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
        | SessionEvent::TurnBoundary { at, .. }
        | SessionEvent::HookRan { at, .. }
        | SessionEvent::StateSnapshot { at, .. } => *at,
    }
}

/// An event as stored, with its assigned monotonic sequence number.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    /// Store-assigned, strictly increasing within `(tenant, session_id)`.
    pub seq: u64,
    pub event: SessionEvent,
}

/// Per-session metadata, for listing without scanning the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub label: Option<String>,
    pub event_count: u64,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
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

    /// Append several events in order. Override for a single-transaction batch;
    /// the default loops `append`.
    async fn append_batch(
        &self,
        tenant: &str,
        session_id: &str,
        events: &[SessionEvent],
    ) -> Result<()> {
        for event in events {
            self.append(tenant, session_id, event).await?;
        }
        Ok(())
    }

    /// Read every event for a session, in `seq` order.
    async fn read(&self, tenant: &str, session_id: &str) -> Result<Vec<StoredEvent>>;

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
    /// cursor. Default filters `list_sessions`; override to push the keyset +
    /// LIMIT into the store.
    async fn list_sessions_page(
        &self,
        tenant: &str,
        after: Option<(DateTime<Utc>, String)>,
        limit: usize,
    ) -> Result<Vec<SessionMeta>> {
        let all = self.list_sessions(tenant).await?;
        Ok(all
            .into_iter()
            .filter(|m| match &after {
                Some((at, id)) => (m.last_activity, m.session_id.as_str()) < (*at, id.as_str()),
                None => true,
            })
            .take(limit)
            .collect())
    }

    /// Read one session's metadata without scanning the event log.
    async fn session_meta(&self, tenant: &str, session_id: &str) -> Result<Option<SessionMeta>>;

    /// Set the durable session label. Implementations should upsert session
    /// metadata so titled empty sessions are materialized.
    async fn set_label(&self, tenant: &str, session_id: &str, label: Option<&str>) -> Result<()>;

    /// Delete a session and all its events.
    async fn delete_session(&self, tenant: &str, session_id: &str) -> Result<()>;

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
