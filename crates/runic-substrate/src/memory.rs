//! In-RAM backends (tests / ephemeral, no-persistence mode):
//! [`MemoryArtifactStore`] for media bytes and [`MemorySessionStore`] for the
//! session event log. Nothing here survives a restart.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};

use runic_state::SessionEvent;
use runic_types::Role;

use tokio::sync::RwLock;

use crate::artifacts::{Artifact, ArtifactSource, ArtifactStore, new_artifact_id};
use crate::sessions::event_at;
use crate::{ChatHit, Error, Result, SessionMeta, SessionStore, StoredEvent};

/// Bytes live in a map; nothing persists.
#[derive(Default)]
pub struct MemoryArtifactStore {
    blobs: RwLock<HashMap<String, (Artifact, Vec<u8>)>>,
    index: RwLock<HashMap<(String, String), Vec<String>>>,
}

impl MemoryArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ArtifactStore for MemoryArtifactStore {
    async fn put(
        &self,
        tenant: &str,
        session_id: &str,
        mime_type: &str,
        source: ArtifactSource,
        bytes: &[u8],
    ) -> Result<Artifact> {
        let artifact = Artifact {
            id: new_artifact_id(),
            mime_type: mime_type.to_string(),
            size: bytes.len() as u64,
            source,
            created_at: Utc::now(),
        };
        self.blobs
            .write()
            .await
            .insert(artifact.id.clone(), (artifact.clone(), bytes.to_vec()));
        self.index
            .write()
            .await
            .entry((tenant.to_string(), session_id.to_string()))
            .or_default()
            .push(artifact.id.clone());
        Ok(artifact)
    }

    async fn get(&self, id: &str) -> Result<Vec<u8>> {
        self.blobs
            .read()
            .await
            .get(id)
            .map(|(_, b)| b.clone())
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    async fn head(&self, id: &str) -> Result<Artifact> {
        self.blobs
            .read()
            .await
            .get(id)
            .map(|(m, _)| m.clone())
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    async fn list(&self, tenant: &str, session_id: &str) -> Result<Vec<Artifact>> {
        let index = self.index.read().await;
        let blobs = self.blobs.read().await;
        let ids = index
            .get(&(tenant.to_string(), session_id.to_string()))
            .cloned()
            .unwrap_or_default();
        Ok(ids
            .iter()
            .filter_map(|id| blobs.get(id).map(|(m, _)| m.clone()))
            .collect())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.blobs.write().await.remove(id);
        Ok(())
    }
}

// ─── MemorySessionStore ──────────────────────────────────────────────────────

struct SessionRec {
    events: Vec<StoredEvent>,
    label: Option<String>,
    created_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    summary: Summary,
}

#[derive(Default)]
struct Summary {
    run_count: u64,
    errored_runs: u64,
    input_tokens: u64,
    output_tokens: u64,
    last_run_status: Option<String>,
    last_run_at: Option<DateTime<Utc>>,
}

impl Summary {
    fn apply(&mut self, event: &SessionEvent) {
        let delta = crate::sessions::summary_delta(event);
        self.run_count += delta.runs;
        self.errored_runs += delta.errored;
        self.input_tokens += delta.input_tokens;
        self.output_tokens += delta.output_tokens;
        if let Some(status) = delta.last_run_status {
            self.last_run_status = Some(status.to_string());
        }
        if let Some(at) = delta.last_run_at {
            self.last_run_at = Some(at);
        }
    }
}

struct ThreadLease {
    claimed_by: String,
    expires_at: DateTime<Utc>,
}

/// In-RAM [`SessionStore`] — the event log in a map. Tests / ephemeral mode;
/// nothing survives a restart.
#[derive(Default)]
pub struct MemorySessionStore {
    sessions: RwLock<HashMap<(String, String), SessionRec>>,
    runs: RwLock<HashMap<String, crate::RunRecord>>,
    steering: RwLock<HashMap<String, Vec<String>>>,
    thread_leases: RwLock<HashMap<(String, String), ThreadLease>>,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

fn snippet(text: &str) -> String {
    let t = text.trim();
    let head: String = t.chars().take(200).collect();
    if t.chars().count() > 200 {
        format!("{head}…")
    } else {
        head
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn append(&self, tenant: &str, session_id: &str, event: &SessionEvent) -> Result<u64> {
        let mut sessions = self.sessions.write().await;
        let rec = sessions
            .entry((tenant.to_string(), session_id.to_string()))
            .or_insert_with(|| SessionRec {
                events: Vec::new(),
                label: None,
                created_at: event_at(event),
                last_activity: event_at(event),
                summary: Summary::default(),
            });
        let seq = rec.events.len() as u64 + 1;
        rec.last_activity = event_at(event);
        rec.summary.apply(event);
        rec.events.push(StoredEvent {
            seq,
            event: event.clone(),
        });
        Ok(seq)
    }

    async fn append_batch(
        &self,
        tenant: &str,
        session_id: &str,
        events: &[SessionEvent],
    ) -> Result<()> {
        let Some(first) = events.first() else {
            return Ok(());
        };
        let mut sessions = self.sessions.write().await;
        let rec = sessions
            .entry((tenant.to_string(), session_id.to_string()))
            .or_insert_with(|| SessionRec {
                events: Vec::new(),
                label: None,
                created_at: event_at(first),
                last_activity: event_at(first),
                summary: Summary::default(),
            });
        for event in events {
            let seq = rec.events.len() as u64 + 1;
            rec.last_activity = event_at(event);
            rec.summary.apply(event);
            rec.events.push(StoredEvent {
                seq,
                event: event.clone(),
            });
        }
        Ok(())
    }

    async fn read(&self, tenant: &str, session_id: &str) -> Result<Vec<StoredEvent>> {
        Ok(self
            .sessions
            .read()
            .await
            .get(&(tenant.to_string(), session_id.to_string()))
            .map(|r| r.events.clone())
            .unwrap_or_default())
    }

    async fn read_after(
        &self,
        tenant: &str,
        session_id: &str,
        after_seq: u64,
    ) -> Result<Vec<StoredEvent>> {
        Ok(self
            .sessions
            .read()
            .await
            .get(&(tenant.to_string(), session_id.to_string()))
            .map(|r| {
                r.events
                    .iter()
                    .filter(|e| e.seq > after_seq)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn list_sessions(&self, tenant: &str) -> Result<Vec<SessionMeta>> {
        let sessions = self.sessions.read().await;
        let mut out: Vec<SessionMeta> = sessions
            .iter()
            .filter(|((t, _), _)| t == tenant)
            .map(|((_, sid), rec)| SessionMeta {
                session_id: sid.clone(),
                label: rec.label.clone(),
                event_count: rec.events.len() as u64,
                created_at: rec.created_at,
                last_activity: rec.last_activity,
                run_count: rec.summary.run_count,
                errored_runs: rec.summary.errored_runs,
                input_tokens: rec.summary.input_tokens,
                output_tokens: rec.summary.output_tokens,
                last_run_status: rec.summary.last_run_status.clone(),
                last_run_at: rec.summary.last_run_at,
            })
            .collect();
        out.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        Ok(out)
    }

    async fn session_meta(&self, tenant: &str, session_id: &str) -> Result<Option<SessionMeta>> {
        Ok(self
            .sessions
            .read()
            .await
            .get(&(tenant.to_string(), session_id.to_string()))
            .map(|rec| SessionMeta {
                session_id: session_id.to_string(),
                label: rec.label.clone(),
                event_count: rec.events.len() as u64,
                created_at: rec.created_at,
                last_activity: rec.last_activity,
                run_count: rec.summary.run_count,
                errored_runs: rec.summary.errored_runs,
                input_tokens: rec.summary.input_tokens,
                output_tokens: rec.summary.output_tokens,
                last_run_status: rec.summary.last_run_status.clone(),
                last_run_at: rec.summary.last_run_at,
            }))
    }

    async fn set_label(&self, tenant: &str, session_id: &str, label: Option<&str>) -> Result<()> {
        let now = Utc::now();
        let mut sessions = self.sessions.write().await;
        let rec = sessions
            .entry((tenant.to_string(), session_id.to_string()))
            .or_insert_with(|| SessionRec {
                events: Vec::new(),
                label: None,
                created_at: now,
                last_activity: now,
                summary: Summary::default(),
            });
        rec.label = label.map(str::to_string);
        Ok(())
    }

    async fn delete_session(&self, tenant: &str, session_id: &str) -> Result<()> {
        self.sessions
            .write()
            .await
            .remove(&(tenant.to_string(), session_id.to_string()));
        let dropped: Vec<String> = {
            let mut runs = self.runs.write().await;
            let dropped = runs
                .values()
                .filter(|r| r.tenant == tenant && r.session_id == session_id)
                .map(|r| r.run_id.clone())
                .collect();
            runs.retain(|_, r| !(r.tenant == tenant && r.session_id == session_id));
            dropped
        };
        let mut steering = self.steering.write().await;
        for run_id in dropped {
            steering.remove(&run_id);
        }
        self.thread_leases
            .write()
            .await
            .remove(&(tenant.to_string(), session_id.to_string()));
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
        let now = Utc::now();
        self.runs.write().await.insert(
            run_id.to_string(),
            crate::RunRecord {
                run_id: run_id.to_string(),
                tenant: tenant.to_string(),
                session_id: session_id.to_string(),
                agent: agent.to_string(),
                status: if input.queued {
                    crate::RunStatus::Queued
                } else {
                    crate::RunStatus::Pending
                },
                error: None,
                claimed_by: None,
                lease_expires_at: None,
                input: input.input.clone(),
                context: input.context.clone(),
                cancel_requested: false,
                created_at: now,
                updated_at: now,
            },
        );
        Ok(())
    }

    async fn set_run_status(
        &self,
        run_id: &str,
        status: crate::RunStatus,
        error: Option<&str>,
    ) -> Result<()> {
        let mut runs = self.runs.write().await;
        let Some(rec) = runs.get_mut(run_id) else {
            return Err(Error::NotFound(format!("run {run_id}")));
        };
        rec.status = status;
        rec.error = error.map(str::to_string);
        rec.updated_at = Utc::now();
        Ok(())
    }

    async fn claim_run(
        &self,
        run_id: &str,
        claimed_by: &str,
        lease: chrono::Duration,
    ) -> Result<bool> {
        let mut runs = self.runs.write().await;
        let Some(rec) = runs.get_mut(run_id) else {
            return Ok(false);
        };
        if !matches!(
            rec.status,
            crate::RunStatus::Pending | crate::RunStatus::Queued
        ) || rec.claimed_by.is_some()
        {
            return Ok(false);
        }
        let now = Utc::now();
        rec.status = crate::RunStatus::Running;
        rec.claimed_by = Some(claimed_by.to_string());
        rec.lease_expires_at = Some(now + lease);
        rec.updated_at = now;
        Ok(true)
    }

    async fn heartbeat_run(
        &self,
        run_id: &str,
        claimed_by: &str,
        lease: chrono::Duration,
    ) -> Result<Option<crate::RunSignals>> {
        let mut runs = self.runs.write().await;
        let Some(rec) = runs.get_mut(run_id) else {
            return Ok(None);
        };
        if rec.status != crate::RunStatus::Running || rec.claimed_by.as_deref() != Some(claimed_by)
        {
            return Ok(None);
        }
        let now = Utc::now();
        rec.lease_expires_at = Some(now + lease);
        rec.updated_at = now;
        let steering = self
            .steering
            .write()
            .await
            .remove(run_id)
            .unwrap_or_default();
        Ok(Some(crate::RunSignals {
            cancel_requested: rec.cancel_requested,
            steering,
        }))
    }

    async fn request_cancel_run(&self, tenant: &str, run_id: &str) -> Result<bool> {
        let mut runs = self.runs.write().await;
        let Some(rec) = runs.get_mut(run_id) else {
            return Ok(false);
        };
        if rec.tenant != tenant || rec.status.is_terminal() {
            return Ok(false);
        }
        let dormant = rec.status == crate::RunStatus::Paused
            || (rec.status == crate::RunStatus::Queued && rec.claimed_by.is_none());
        if dormant {
            rec.status = crate::RunStatus::Cancelled;
        } else {
            rec.cancel_requested = true;
        }
        rec.updated_at = Utc::now();
        Ok(true)
    }

    async fn push_steering(&self, tenant: &str, run_id: &str, text: &str) -> Result<bool> {
        let runs = self.runs.read().await;
        let Some(rec) = runs.get(run_id) else {
            return Ok(false);
        };
        if rec.tenant != tenant || rec.status.is_terminal() {
            return Ok(false);
        }
        self.steering
            .write()
            .await
            .entry(run_id.to_string())
            .or_default()
            .push(text.to_string());
        Ok(true)
    }

    async fn claim_thread(
        &self,
        tenant: &str,
        session_id: &str,
        claimed_by: &str,
        lease: chrono::Duration,
    ) -> Result<bool> {
        let now = Utc::now();
        let mut leases = self.thread_leases.write().await;
        let key = (tenant.to_string(), session_id.to_string());
        match leases.get(&key) {
            Some(l) if l.claimed_by != claimed_by && l.expires_at > now => Ok(false),
            _ => {
                leases.insert(
                    key,
                    ThreadLease {
                        claimed_by: claimed_by.to_string(),
                        expires_at: now + lease,
                    },
                );
                Ok(true)
            }
        }
    }

    async fn extend_thread_lease(
        &self,
        tenant: &str,
        session_id: &str,
        claimed_by: &str,
        lease: chrono::Duration,
    ) -> Result<bool> {
        let mut leases = self.thread_leases.write().await;
        let key = (tenant.to_string(), session_id.to_string());
        match leases.get_mut(&key) {
            Some(l) if l.claimed_by == claimed_by => {
                l.expires_at = Utc::now() + lease;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn release_thread(&self, tenant: &str, session_id: &str, claimed_by: &str) -> Result<()> {
        let mut leases = self.thread_leases.write().await;
        let key = (tenant.to_string(), session_id.to_string());
        if leases.get(&key).is_some_and(|l| l.claimed_by == claimed_by) {
            leases.remove(&key);
        }
        Ok(())
    }

    async fn reap_expired_runs(&self) -> Result<Vec<crate::RunRecord>> {
        let now = Utc::now();
        let mut reaped = Vec::new();
        {
            let mut runs = self.runs.write().await;
            for rec in runs.values_mut() {
                if rec.status == crate::RunStatus::Running
                    && rec.lease_expires_at.is_some_and(|at| at < now)
                {
                    rec.status = crate::RunStatus::Error;
                    rec.error = Some("lease expired".into());
                    rec.updated_at = now;
                    reaped.push(rec.clone());
                }
            }
        }
        let mut sessions = self.sessions.write().await;
        for run in &reaped {
            if let Some(rec) = sessions.get_mut(&(run.tenant.clone(), run.session_id.clone()))
                && rec.summary.last_run_status.as_deref() == Some("running")
                && rec
                    .summary
                    .last_run_at
                    .is_some_and(|at| at <= run.created_at)
            {
                rec.summary.last_run_status = Some("failed".to_string());
            }
        }
        Ok(reaped)
    }

    async fn claim_next_queued_run(
        &self,
        claimed_by: &str,
        lease: chrono::Duration,
    ) -> Result<Option<crate::RunRecord>> {
        let now = Utc::now();
        let mut runs = self.runs.write().await;
        let next = runs
            .values()
            .filter(|r| r.status == crate::RunStatus::Queued && r.claimed_by.is_none())
            .min_by_key(|r| r.created_at)
            .map(|r| r.run_id.clone());
        let Some(run_id) = next else {
            return Ok(None);
        };
        let rec = runs.get_mut(&run_id).unwrap();
        rec.status = crate::RunStatus::Running;
        rec.claimed_by = Some(claimed_by.to_string());
        rec.lease_expires_at = Some(now + lease);
        rec.updated_at = now;
        Ok(Some(rec.clone()))
    }

    async fn release_run(&self, run_id: &str, claimed_by: &str) -> Result<()> {
        let mut runs = self.runs.write().await;
        if let Some(rec) = runs.get_mut(run_id)
            && rec.claimed_by.as_deref() == Some(claimed_by)
        {
            rec.status = crate::RunStatus::Queued;
            rec.claimed_by = None;
            rec.lease_expires_at = None;
            rec.updated_at = Utc::now();
        }
        Ok(())
    }

    async fn resume_run(&self, tenant: &str, run_id: &str) -> Result<bool> {
        let mut runs = self.runs.write().await;
        let Some(rec) = runs.get_mut(run_id) else {
            return Ok(false);
        };
        if rec.tenant != tenant || rec.status != crate::RunStatus::Paused {
            return Ok(false);
        }
        rec.status = crate::RunStatus::Queued;
        rec.claimed_by = None;
        rec.lease_expires_at = None;
        rec.updated_at = Utc::now();
        Ok(true)
    }

    async fn deliver_and_resume(
        &self,
        tenant: &str,
        run_id: &str,
        event: &SessionEvent,
    ) -> Result<bool> {
        let mut runs = self.runs.write().await;
        let Some(rec) = runs.get_mut(run_id) else {
            return Ok(false);
        };
        if rec.tenant != tenant || rec.status != crate::RunStatus::Paused {
            return Ok(false);
        }
        let session_id = rec.session_id.clone();
        {
            let mut sessions = self.sessions.write().await;
            let srec = sessions
                .entry((tenant.to_string(), session_id))
                .or_insert_with(|| SessionRec {
                    events: Vec::new(),
                    label: None,
                    created_at: event_at(event),
                    last_activity: event_at(event),
                    summary: Summary::default(),
                });
            let seq = srec.events.len() as u64 + 1;
            srec.last_activity = event_at(event);
            srec.summary.apply(event);
            srec.events.push(StoredEvent {
                seq,
                event: event.clone(),
            });
        }
        rec.status = crate::RunStatus::Queued;
        rec.claimed_by = None;
        rec.lease_expires_at = None;
        rec.updated_at = Utc::now();
        Ok(true)
    }

    async fn get_run(&self, tenant: &str, run_id: &str) -> Result<Option<crate::RunRecord>> {
        Ok(self
            .runs
            .read()
            .await
            .get(run_id)
            .filter(|r| r.tenant == tenant)
            .cloned())
    }

    async fn list_runs(
        &self,
        tenant: &str,
        session_id: &str,
        limit: usize,
        before: Option<(chrono::DateTime<chrono::Utc>, String)>,
    ) -> Result<Vec<crate::RunRecord>> {
        let mut records: Vec<crate::RunRecord> = self
            .runs
            .read()
            .await
            .values()
            .filter(|r| {
                r.tenant == tenant
                    && r.session_id == session_id
                    && before.as_ref().is_none_or(|(cut_at, cut_id)| {
                        (&r.created_at, &r.run_id) < (cut_at, cut_id)
                    })
            })
            .cloned()
            .collect();
        records.sort_by(|a, b| (&b.created_at, &b.run_id).cmp(&(&a.created_at, &a.run_id)));
        records.truncate(limit);
        Ok(records)
    }

    async fn latest_active_run(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Option<crate::RunRecord>> {
        let runs = self.runs.read().await;
        Ok(runs
            .values()
            .filter(|r| r.tenant == tenant && r.session_id == session_id && !r.status.is_terminal())
            .max_by_key(|r| (r.status == crate::RunStatus::Running, r.created_at))
            .cloned())
    }

    async fn latest_run(&self, tenant: &str, session_id: &str) -> Result<Option<crate::RunRecord>> {
        Ok(self
            .runs
            .read()
            .await
            .values()
            .filter(|r| r.tenant == tenant && r.session_id == session_id)
            .max_by_key(|r| r.created_at)
            .cloned())
    }

    async fn search(
        &self,
        tenant: &str,
        query: &str,
        limit: usize,
        exclude_session: Option<&str>,
    ) -> Result<Vec<ChatHit>> {
        let q = query.to_lowercase();
        let sessions = self.sessions.read().await;
        let mut hits = Vec::new();
        for ((t, sid), rec) in sessions.iter() {
            if t != tenant || Some(sid.as_str()) == exclude_session {
                continue;
            }
            for stored in &rec.events {
                if let SessionEvent::Message { msg, .. } = &stored.event {
                    let role = match msg.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::System => continue,
                    };
                    let text = msg.content.text_content();
                    if text.to_lowercase().contains(&q) {
                        hits.push(ChatHit {
                            session_id: sid.clone(),
                            seq: stored.seq,
                            role: role.to_string(),
                            snippet: snippet(&text),
                            at: event_at(&stored.event),
                        });
                    }
                }
            }
        }
        hits.truncate(limit);
        Ok(hits)
    }

    async fn cleanup_stale(&self, ttl: Duration) -> Result<u64> {
        let cutoff = Utc::now() - ttl;
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|_, rec| rec.last_activity >= cutoff);
        Ok((before - sessions.len()) as u64)
    }

    async fn list_tenants(&self) -> Result<Vec<String>> {
        let sessions = self.sessions.read().await;
        let mut tenants: Vec<String> = sessions.keys().map(|(t, _)| t.clone()).collect();
        tenants.sort();
        tenants.dedup();
        Ok(tenants)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_artifact_roundtrip() {
        let s = MemoryArtifactStore::new();
        let a = s
            .put(
                "t",
                "sess",
                "application/pdf",
                ArtifactSource::UserUpload,
                b"%PDF",
            )
            .await
            .unwrap();
        assert_eq!(s.get(&a.id).await.unwrap(), b"%PDF");
        assert_eq!(s.head(&a.id).await.unwrap().mime_type, "application/pdf");
        assert_eq!(s.list("t", "sess").await.unwrap().len(), 1);
        assert!(s.list("t", "other").await.unwrap().is_empty());
        s.delete(&a.id).await.unwrap();
        assert!(matches!(s.get(&a.id).await, Err(Error::NotFound(_))));
    }

    // ── MemorySessionStore ──────────────────────────────────────────────────

    use runic_types::Message;

    fn user_msg(run: &str, text: &str) -> SessionEvent {
        SessionEvent::Message {
            run_id: run.to_string(),
            msg: Message::user(text),
            at: Utc::now(),
        }
    }
    fn assistant_msg(run: &str, text: &str) -> SessionEvent {
        SessionEvent::Message {
            run_id: run.to_string(),
            msg: Message::assistant(text),
            at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn append_assigns_monotonic_seq_and_read_is_ordered() {
        let s = MemorySessionStore::new();
        assert_eq!(
            s.append("t", "s1", &user_msg("r1", "hello")).await.unwrap(),
            1
        );
        assert_eq!(
            s.append("t", "s1", &assistant_msg("r1", "hi"))
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            s.append("t", "s1", &user_msg("r1", "bye")).await.unwrap(),
            3
        );

        let events = s.read("t", "s1").await.unwrap();
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        // tailing
        let tail = s.read_after("t", "s1", 1).await.unwrap();
        assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);
        // unknown session is empty, not an error
        assert!(s.read("t", "nope").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_sessions_is_tenant_scoped_with_metadata() {
        let s = MemorySessionStore::new();
        s.append("alice", "a1", &user_msg("r", "x")).await.unwrap();
        s.append("alice", "a1", &user_msg("r", "y")).await.unwrap();
        s.append("alice", "a2", &user_msg("r", "z")).await.unwrap();
        s.append("bob", "b1", &user_msg("r", "secret"))
            .await
            .unwrap();

        let alice = s.list_sessions("alice").await.unwrap();
        assert_eq!(alice.len(), 2);
        assert!(
            alice
                .iter()
                .all(|m| m.session_id == "a1" || m.session_id == "a2")
        );
        let a1 = alice.iter().find(|m| m.session_id == "a1").unwrap();
        assert_eq!(a1.event_count, 2);
        // never leaks Bob's
        assert_eq!(s.list_sessions("bob").await.unwrap().len(), 1);
        assert_eq!(s.list_tenants().await.unwrap(), vec!["alice", "bob"]);
    }

    #[tokio::test]
    async fn delete_session_removes_it() {
        let s = MemorySessionStore::new();
        s.append("t", "s1", &user_msg("r", "x")).await.unwrap();
        s.delete_session("t", "s1").await.unwrap();
        assert!(s.read("t", "s1").await.unwrap().is_empty());
        assert!(s.list_sessions("t").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_is_tenant_scoped_and_excludes_current() {
        let s = MemorySessionStore::new();
        s.append("acme", "past", &user_msg("r", "deploy the staging server"))
            .await
            .unwrap();
        s.append("acme", "past", &assistant_msg("r", "done deploying"))
            .await
            .unwrap();
        s.append("acme", "current", &user_msg("r", "deploy again"))
            .await
            .unwrap();
        s.append("other", "x", &user_msg("r", "deploy in another tenant"))
            .await
            .unwrap();

        // tenant acme, excluding the current session → only the "past" hits
        let hits = s
            .search("acme", "deploy", 10, Some("current"))
            .await
            .unwrap();
        assert!(hits.iter().all(|h| h.session_id == "past"));
        assert!(hits.iter().any(|h| h.snippet.contains("deploy")));
        // never crosses tenants
        assert!(
            s.search("acme", "another tenant", 10, None)
                .await
                .unwrap()
                .is_empty()
        );
        // limit is honored
        assert_eq!(s.search("acme", "deploy", 1, None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cleanup_stale_drops_only_old_sessions() {
        let s = MemorySessionStore::new();
        s.append("t", "fresh", &user_msg("r", "x")).await.unwrap();
        // negative ttl ⇒ cutoff is in the future ⇒ everything is "stale"
        let removed = s.cleanup_stale(Duration::seconds(-1)).await.unwrap();
        assert_eq!(removed, 1);
        assert!(s.list_sessions("t").await.unwrap().is_empty());
    }
}
