//! In-RAM backends (tests / ephemeral, no-persistence mode):
//! [`MemoryArtifactStore`] for media bytes and [`MemorySessionStore`] for the
//! session event log. Nothing here survives a restart.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};

use runic_state::SessionEvent;
use runic_types::Role;

use crate::artifacts::{Artifact, ArtifactSource, ArtifactStore, new_artifact_id};
use crate::sessions::event_at;
use crate::{ChatHit, Error, Result, SessionMeta, SessionStore, StoredEvent};

/// Bytes live in a map; nothing persists.
#[derive(Default)]
pub struct MemoryArtifactStore {
    blobs: Mutex<HashMap<String, (Artifact, Vec<u8>)>>,
    index: Mutex<HashMap<(String, String), Vec<String>>>,
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
            .lock()
            .unwrap()
            .insert(artifact.id.clone(), (artifact.clone(), bytes.to_vec()));
        self.index
            .lock()
            .unwrap()
            .entry((tenant.to_string(), session_id.to_string()))
            .or_default()
            .push(artifact.id.clone());
        Ok(artifact)
    }

    async fn get(&self, id: &str) -> Result<Vec<u8>> {
        self.blobs
            .lock()
            .unwrap()
            .get(id)
            .map(|(_, b)| b.clone())
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    async fn head(&self, id: &str) -> Result<Artifact> {
        self.blobs
            .lock()
            .unwrap()
            .get(id)
            .map(|(m, _)| m.clone())
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    async fn list(&self, tenant: &str, session_id: &str) -> Result<Vec<Artifact>> {
        let index = self.index.lock().unwrap();
        let blobs = self.blobs.lock().unwrap();
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
        self.blobs.lock().unwrap().remove(id);
        Ok(())
    }
}

// ─── MemorySessionStore ──────────────────────────────────────────────────────

struct SessionRec {
    events: Vec<StoredEvent>,
    created_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,
}

/// In-RAM [`SessionStore`] — the event log in a map. Tests / ephemeral mode;
/// nothing survives a restart.
#[derive(Default)]
pub struct MemorySessionStore {
    sessions: Mutex<HashMap<(String, String), SessionRec>>,
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
        let mut sessions = self.sessions.lock().unwrap();
        let rec = sessions
            .entry((tenant.to_string(), session_id.to_string()))
            .or_insert_with(|| SessionRec {
                events: Vec::new(),
                created_at: event_at(event),
                last_activity: event_at(event),
            });
        let seq = rec.events.len() as u64 + 1;
        rec.last_activity = event_at(event);
        rec.events.push(StoredEvent {
            seq,
            event: event.clone(),
        });
        Ok(seq)
    }

    async fn read(&self, tenant: &str, session_id: &str) -> Result<Vec<StoredEvent>> {
        Ok(self
            .sessions
            .lock()
            .unwrap()
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
            .lock()
            .unwrap()
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
        let sessions = self.sessions.lock().unwrap();
        let mut out: Vec<SessionMeta> = sessions
            .iter()
            .filter(|((t, _), _)| t == tenant)
            .map(|((_, sid), rec)| SessionMeta {
                session_id: sid.clone(),
                label: None,
                event_count: rec.events.len() as u64,
                created_at: rec.created_at,
                last_activity: rec.last_activity,
            })
            .collect();
        out.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        Ok(out)
    }

    async fn delete_session(&self, tenant: &str, session_id: &str) -> Result<()> {
        self.sessions
            .lock()
            .unwrap()
            .remove(&(tenant.to_string(), session_id.to_string()));
        Ok(())
    }

    async fn search(
        &self,
        tenant: &str,
        query: &str,
        limit: usize,
        exclude_session: Option<&str>,
    ) -> Result<Vec<ChatHit>> {
        let q = query.to_lowercase();
        let sessions = self.sessions.lock().unwrap();
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
        let mut sessions = self.sessions.lock().unwrap();
        let before = sessions.len();
        sessions.retain(|_, rec| rec.last_activity >= cutoff);
        Ok((before - sessions.len()) as u64)
    }

    async fn list_tenants(&self) -> Result<Vec<String>> {
        let sessions = self.sessions.lock().unwrap();
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
