//! In-RAM session event log (tests / ephemeral, no-persistence mode).
//! Nothing here survives a restart.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};

use crate::SessionEvent;
use runic_types::Role;

use tokio::sync::RwLock;

use crate::sessions::event_at;
use crate::{ChatHit, Error, Result, SessionMeta, SessionStore, StoredEvent};

// ─── MemorySessionStore ──────────────────────────────────────────────────────

struct SessionRec {
    events: Vec<StoredEvent>,
    label: Option<String>,
    created_at: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    summary: Summary,
    agent: Option<String>,
    parent_session: Option<String>,
}

impl SessionRec {
    fn new(at: DateTime<Utc>) -> Self {
        Self {
            events: Vec::new(),
            label: None,
            created_at: at,
            last_activity: at,
            summary: Summary::default(),
            agent: None,
            parent_session: None,
        }
    }

    fn meta(&self, session_id: &str) -> SessionMeta {
        SessionMeta {
            session_id: session_id.to_string(),
            label: self.label.clone(),
            event_count: self.events.len() as u64,
            created_at: self.created_at,
            last_activity: self.last_activity,
            agent: self.agent.clone(),
            parent_session: self.parent_session.clone(),
            run_count: self.summary.run_count,
            errored_runs: self.summary.errored_runs,
            input_tokens: self.summary.input_tokens,
            output_tokens: self.summary.output_tokens,
            last_run_status: self.summary.last_run_status.clone(),
            last_run_at: self.summary.last_run_at,
        }
    }
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

/// In-RAM [`SessionStore`] — the event log in a map. Tests / ephemeral mode;
/// nothing survives a restart.
#[derive(Default)]
pub struct MemorySessionStore {
    sessions: RwLock<HashMap<(String, String), SessionRec>>,
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
            .or_insert_with(|| SessionRec::new(event_at(event)));
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
            .or_insert_with(|| SessionRec::new(event_at(first)));
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

    async fn append_batch_strict(
        &self,
        tenant: &str,
        session_id: &str,
        events: &[SessionEvent],
    ) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        let Some(rec) = sessions.get_mut(&(tenant.to_string(), session_id.to_string())) else {
            return Err(Error::NotFound(format!("session {session_id}")));
        };
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
            .map(|((_, sid), rec)| rec.meta(sid))
            .collect();
        out.sort_by(|a, b| (b.last_activity, &b.session_id).cmp(&(a.last_activity, &a.session_id)));
        Ok(out)
    }

    async fn session_meta(&self, tenant: &str, session_id: &str) -> Result<Option<SessionMeta>> {
        Ok(self
            .sessions
            .read()
            .await
            .get(&(tenant.to_string(), session_id.to_string()))
            .map(|rec| rec.meta(session_id)))
    }

    async fn set_label(&self, tenant: &str, session_id: &str, label: Option<&str>) -> Result<()> {
        let now = Utc::now();
        let mut sessions = self.sessions.write().await;
        let rec = sessions
            .entry((tenant.to_string(), session_id.to_string()))
            .or_insert_with(|| SessionRec::new(now));
        rec.label = label.map(str::to_string);
        Ok(())
    }

    async fn create_child_session(
        &self,
        tenant: &str,
        session_id: &str,
        parent_session: &str,
        agent: &str,
    ) -> Result<()> {
        let now = Utc::now();
        let mut sessions = self.sessions.write().await;
        if !sessions.contains_key(&(tenant.to_string(), parent_session.to_string())) {
            return Err(Error::NotFound(format!("parent session {parent_session}")));
        }
        let rec = sessions
            .entry((tenant.to_string(), session_id.to_string()))
            .or_insert_with(|| SessionRec::new(now));
        rec.agent = Some(agent.to_string());
        rec.parent_session = Some(parent_session.to_string());
        Ok(())
    }

    async fn delete_session(&self, tenant: &str, session_id: &str) -> Result<()> {
        self.sessions
            .write()
            .await
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
    use crate::SessionScope;

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
    async fn keyset_pagination_survives_identical_last_activity() {
        let s = MemorySessionStore::new();
        let tied_at = Utc::now();
        {
            let mut sessions = s.sessions.write().await;
            for idx in 0..5 {
                sessions.insert(
                    ("t".to_string(), format!("sess-{idx}")),
                    SessionRec::new(tied_at),
                );
            }
        }
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = s
                .list_sessions_page("t", cursor.clone(), 2, SessionScope::All)
                .await
                .unwrap();
            let Some(last) = page.last() else { break };
            cursor = Some((last.last_activity, last.session_id.clone()));
            seen.extend(page.into_iter().map(|m| m.session_id));
        }
        seen.sort();
        assert_eq!(seen, ["sess-0", "sess-1", "sess-2", "sess-3", "sess-4"]);
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
