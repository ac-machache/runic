//! `AgentState` — the agent's working state for one conversation.

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use runic_types::Message;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

use crate::event::SessionEvent;

pub const EVENT_BROADCAST_CAPACITY: usize = 1024;

pub fn new_run_id() -> String {
    format!("r-{}", uuid::Uuid::new_v4().simple())
}

pub const MAX_STATE_KEY_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidStateKey(pub &'static str);

impl std::fmt::Display for InvalidStateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid state key: {}", self.0)
    }
}

impl std::error::Error for InvalidStateKey {}

pub fn validate_state_key(key: &str) -> Result<(), InvalidStateKey> {
    if key.is_empty() {
        return Err(InvalidStateKey("must not be empty"));
    }
    if key.len() > MAX_STATE_KEY_BYTES {
        return Err(InvalidStateKey("exceeds 256 bytes"));
    }
    if key.chars().any(char::is_control) {
        return Err(InvalidStateKey("must not contain control characters"));
    }
    if key.split(['/', '\\']).any(|segment| segment == "..") {
        return Err(InvalidStateKey("must not contain '..' segments"));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct PersistSink {
    tx: mpsc::UnboundedSender<Arc<SessionEvent>>,
    enqueued: Arc<AtomicU64>,
}

impl PersistSink {
    pub fn new(tx: mpsc::UnboundedSender<Arc<SessionEvent>>) -> Self {
        Self {
            tx,
            enqueued: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn send(&self, ev: Arc<SessionEvent>) {
        self.enqueued.fetch_add(1, Ordering::SeqCst);
        let _ = self.tx.send(ev);
    }

    pub fn enqueued(&self) -> Arc<AtomicU64> {
        self.enqueued.clone()
    }
}

#[derive(Default, Clone)]
pub struct RunTimeContext {
    ctx: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl RunTimeContext {
    pub fn insert<T: 'static + Send + Sync>(&mut self, value: T) {
        self.ctx.insert(TypeId::of::<T>(), Arc::new(value));
    }

    pub fn insert_arc<T: 'static + Send + Sync>(&mut self, value: Arc<T>) {
        self.ctx.insert(TypeId::of::<T>(), value);
    }

    pub fn get<T: 'static + Send + Sync>(&self) -> Option<Arc<T>> {
        self.ctx
            .get(&TypeId::of::<T>())
            .and_then(|v| v.clone().downcast::<T>().ok())
    }

    pub fn snapshot(&self) -> HashMap<TypeId, Arc<dyn Any + Send + Sync>> {
        self.ctx.clone()
    }
}

impl std::fmt::Debug for RunTimeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunTimeContext")
            .field("entries", &self.ctx.len())
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentState {
    pub user_id: String,

    pub session_id: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    pub system_prompt: String,

    #[serde(default)]
    stats: crate::stats::ThreadStats,

    #[serde(default)]
    tasks: HashMap<String, crate::tasks::TaskRecord>,

    #[serde(default)]
    data: serde_json::Map<String, serde_json::Value>,

    events: Vec<SessionEvent>,

    #[serde(skip, default)]
    pub runtime: RunTimeContext,

    #[serde(skip, default)]
    pub config: serde_json::Map<String, serde_json::Value>,

    #[serde(skip, default)]
    events_tx: Option<broadcast::Sender<Arc<SessionEvent>>>,

    #[serde(skip, default)]
    persist_tx: Option<PersistSink>,

    #[serde(skip, default)]
    messages: Vec<Message>,
}

impl AgentState {
    pub fn new(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        system_prompt: impl Into<String>,
    ) -> Self {
        Self {
            user_id: user_id.into(),
            session_id: session_id.into(),
            label: None,
            system_prompt: system_prompt.into(),
            stats: crate::stats::ThreadStats::default(),
            tasks: HashMap::new(),
            data: serde_json::Map::new(),
            events: Vec::new(),
            runtime: RunTimeContext::default(),
            config: serde_json::Map::new(),
            events_tx: None,
            persist_tx: None,
            messages: Vec::new(),
        }
    }

    pub fn config(&self, key: &str) -> Option<&serde_json::Value> {
        self.config.get(key)
    }

    pub fn set_events_tx(&mut self, tx: broadcast::Sender<Arc<SessionEvent>>) {
        self.events_tx = Some(tx);
    }

    pub fn set_persist_tx(&mut self, sink: PersistSink) {
        self.persist_tx = Some(sink);
    }

    pub fn subscribe_events(&self) -> Option<broadcast::Receiver<Arc<SessionEvent>>> {
        self.events_tx.as_ref().map(|tx| tx.subscribe())
    }

    pub fn push_event(&mut self, ev: SessionEvent) {
        if self.events_tx.is_some() || self.persist_tx.is_some() {
            let shared = Arc::new(ev.clone());
            if let Some(tx) = &self.events_tx {
                let _ = tx.send(shared.clone());
            }
            if let Some(sink) = &self.persist_tx {
                sink.send(shared);
            }
        }
        self.fold_event(ev);
    }

    /// Fold without fanning to the sinks — for events that are already
    /// persisted (replay, or a tool's out-of-dispatch emission).
    pub fn fold_event(&mut self, ev: SessionEvent) {
        self.stats.fold(&ev);
        match &ev {
            SessionEvent::Message { msg, .. } => self.messages.push(msg.clone()),
            SessionEvent::TaskSpawned {
                task_id,
                agent,
                prompt,
                at,
                ..
            } => {
                self.tasks.insert(
                    task_id.clone(),
                    crate::tasks::TaskRecord {
                        task_id: task_id.clone(),
                        agent: agent.clone(),
                        prompt: prompt.clone(),
                        status: crate::tasks::TaskStatus::Running,
                        result: None,
                        spawned_at: *at,
                        finished_at: None,
                    },
                );
            }
            SessionEvent::TaskFinished {
                task_id,
                status,
                result,
                at,
                ..
            } => {
                if let Some(record) = self.tasks.get_mut(task_id) {
                    record.status = *status;
                    record.result = result.clone();
                    record.finished_at = Some(*at);
                }
            }
            SessionEvent::StateUpdated { key, value, .. } => {
                self.data.insert(key.clone(), value.clone());
            }
            SessionEvent::StateSnapshot {
                messages,
                run_id,
                open_tasks,
                data,
                ..
            } => {
                self.messages = messages.clone();
                if let Some(open) = open_tasks {
                    self.tasks = open
                        .iter()
                        .map(|t| (t.task_id.clone(), t.clone()))
                        .collect();
                }
                if let Some(data) = data {
                    self.data = data.clone();
                }
                // Pre-snapshot events leave RAM; the store keeps the full log.
                // The in-flight run's events stay so run bookkeeping works.
                let cut = self.events.iter().rposition(
                    |e| matches!(e, SessionEvent::RunStart { run_id: r, .. } if r == run_id),
                );
                match cut {
                    Some(i) => {
                        self.events.drain(..i);
                    }
                    None => self.events.clear(),
                }
            }
            _ => {}
        }
        self.events.push(ev);
    }

    pub fn update(
        &mut self,
        key: impl Into<String>,
        value: serde_json::Value,
    ) -> Result<(), InvalidStateKey> {
        let key = key.into();
        validate_state_key(&key)?;
        let run_id = self
            .current_run()
            .map(|r| r.id.clone())
            .unwrap_or_else(|| "update".to_string());
        self.push_event(SessionEvent::StateUpdated {
            run_id,
            key,
            value,
            at: Utc::now(),
        });
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.data.get(key)
    }

    pub fn data(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.data
    }

    pub fn stats(&self) -> &crate::stats::ThreadStats {
        &self.stats
    }

    pub fn tasks(&self) -> &HashMap<String, crate::tasks::TaskRecord> {
        &self.tasks
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    pub fn open_tasks(&self) -> Vec<crate::tasks::TaskRecord> {
        let mut open: Vec<_> = self
            .tasks
            .values()
            .filter(|t| t.status == crate::tasks::TaskStatus::Running)
            .cloned()
            .collect();
        open.sort_by(|a, b| a.spawned_at.cmp(&b.spawned_at));
        open
    }

    pub fn persist_sink(&self) -> Option<PersistSink> {
        self.persist_tx.clone()
    }

    pub fn events_sender(&self) -> Option<broadcast::Sender<Arc<SessionEvent>>> {
        self.events_tx.clone()
    }

    pub fn messages_for_provider(&self) -> &[Message] {
        &self.messages
    }

    /// Grouped view of runs, derived from the log. Cheap, on demand.
    pub fn runs(&self) -> Vec<RunView<'_>> {
        let mut views: Vec<RunView<'_>> = Vec::new();
        let mut index: HashMap<&str, usize> = HashMap::new();
        for ev in &self.events {
            let id = ev.run_id();
            let i = *index.entry(id).or_insert_with(|| {
                views.push(RunView {
                    id: id.to_string(),
                    started_at: None,
                    ended_at: None,
                    events: Vec::new(),
                });
                views.len() - 1
            });
            views[i].events.push(ev);
            match ev {
                SessionEvent::RunStart { at, .. } => views[i].started_at = Some(*at),
                SessionEvent::RunEnd { at, .. } => views[i].ended_at = Some(*at),
                _ => {}
            }
        }
        views
    }

    /// The most recent run with a `RunStart` but no `RunEnd` — found by scanning
    /// from the end, without materializing every run.
    pub fn current_run(&self) -> Option<RunView<'_>> {
        let mut ended: HashSet<&str> = HashSet::new();
        let mut current: Option<&str> = None;
        for ev in self.events.iter().rev() {
            match ev {
                SessionEvent::RunEnd { run_id, .. } => {
                    ended.insert(run_id);
                }
                SessionEvent::RunStart { run_id, .. } => {
                    if !ended.contains(run_id.as_str()) {
                        current = Some(run_id);
                        break;
                    }
                }
                _ => {}
            }
        }
        let id = current?;
        let events: Vec<&SessionEvent> = self.events.iter().filter(|e| e.run_id() == id).collect();
        let started_at = events.iter().find_map(|e| match e {
            SessionEvent::RunStart { at, .. } => Some(*at),
            _ => None,
        });
        Some(RunView {
            id: id.to_string(),
            started_at,
            ended_at: None,
            events,
        })
    }

    /// Execution tree over the in-RAM working set; the store's full log gives
    /// complete history through the same projection.
    pub fn timeline(&self) -> Vec<crate::timeline::RunTrace> {
        crate::timeline::project(&self.events)
    }

    pub fn timeline_for(&self, run_id: &str) -> Option<crate::timeline::RunTrace> {
        crate::timeline::project(self.events.iter().filter(|e| e.run_id() == run_id))
            .into_iter()
            .next()
    }

    /// Most recent assistant text in the log (e.g. the final answer).
    pub fn last_assistant_text(&self) -> Option<String> {
        for msg in self.messages.iter().rev() {
            if msg.role == runic_types::Role::Assistant {
                let t = msg.content.text_content();
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
        None
    }
}

// ─── Run view (derived) ──────────────────────────────────────────────────────

/// A read-only slice of the log for one run.
pub struct RunView<'a> {
    pub id: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub events: Vec<&'a SessionEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookLifecycle;

    fn message(text: &str, user: bool) -> SessionEvent {
        let msg = if user {
            Message::user(text)
        } else {
            Message::assistant(text)
        };
        SessionEvent::Message {
            run_id: "r".into(),
            msg,
            at: Utc::now(),
        }
    }

    #[test]
    fn push_event_folds_messages_and_skips_non_messages() {
        let mut state = AgentState::new("u", "s", "sys");
        state.push_event(SessionEvent::RunStart {
            run_id: "r".into(),
            agent: None,
            audit: None,
            at: Utc::now(),
        });
        state.push_event(message("hello", true));
        state.push_event(message("hi there", false));

        let msgs = state.messages_for_provider();
        assert_eq!(msgs.len(), 2);
        assert_eq!(state.events().len(), 3);
    }

    #[test]
    fn push_event_records_hook_ran_without_touching_messages() {
        let mut state = AgentState::new("u", "s", "sys");
        state.push_event(message("hello", true));
        state.push_event(SessionEvent::HookFired {
            run_id: "r".into(),
            hook: "guard".into(),
            lifecycle: HookLifecycle::BeforeTool,
            hook_kind: "write".into(),
            outcome: "cancel".into(),
            note: Some("blocked".into()),
            at: Utc::now(),
        });

        assert_eq!(state.messages_for_provider().len(), 1);
        assert_eq!(state.events().len(), 2);
        assert!(matches!(state.events()[1], SessionEvent::HookFired { .. }));
    }

    #[test]
    fn state_snapshot_replaces_the_message_view() {
        let mut state = AgentState::new("u", "s", "sys");
        state.push_event(message("old one", true));
        state.push_event(message("old two", false));
        state.push_event(SessionEvent::StateSnapshot {
            run_id: "r".into(),
            messages: vec![Message::user("compacted")],
            system_prompt: "sys".into(),
            reason: "compaction".into(),
            stats: None,
            open_tasks: None,
            data: None,
            at: Utc::now(),
        });
        state.push_event(message("after", false));

        let msgs = state.messages_for_provider();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content.text_content(), "compacted");
        assert_eq!(msgs[1].content.text_content(), "after");
    }

    #[test]
    fn view_matches_a_full_fold_after_replay_style_pushes() {
        let mut state = AgentState::new("u", "s", "sys");
        for ev in [
            SessionEvent::RunStart {
                run_id: "r".into(),
                agent: None,
                audit: None,
                at: Utc::now(),
            },
            message("a", true),
            message("b", false),
            message("c", true),
        ] {
            state.push_event(ev);
        }
        let folded: Vec<_> = state
            .events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Message { msg, .. } => Some(msg.content.text_content()),
                _ => None,
            })
            .collect();
        let view: Vec<_> = state
            .messages_for_provider()
            .iter()
            .map(|m| m.content.text_content())
            .collect();
        assert_eq!(view, folded);
    }

    #[test]
    fn update_rejects_hostile_keys_and_emits_nothing_for_them() {
        let mut state = AgentState::new("u", "s", "");
        state.update("ok/key", serde_json::json!(1)).unwrap();
        assert!(state.update("", serde_json::json!(1)).is_err());
        assert!(state.update("a\nb", serde_json::json!(1)).is_err());
        assert!(state.update("a\0b", serde_json::json!(1)).is_err());
        assert!(state.update("../escape", serde_json::json!(1)).is_err());
        assert!(
            state
                .update("deep/../escape", serde_json::json!(1))
                .is_err()
        );
        assert!(
            state
                .update("x".repeat(MAX_STATE_KEY_BYTES + 1), serde_json::json!(1))
                .is_err()
        );

        assert_eq!(state.get("ok/key"), Some(&serde_json::json!(1)));
        assert_eq!(state.data().len(), 1);
        assert_eq!(state.events().len(), 1);
    }

    #[test]
    fn runs_group_in_order_and_current_run_is_in_flight() {
        let mut state = AgentState::new("u", "s", "sys");
        state.push_event(SessionEvent::RunStart {
            run_id: "r1".into(),
            agent: None,
            audit: None,
            at: Utc::now(),
        });
        state.push_event(SessionEvent::RunEnd {
            run_id: "r1".into(),
            status: crate::event::RunEndStatus::Completed,
            outcome: crate::event::RunOutcome::default(),
            at: Utc::now(),
        });
        state.push_event(SessionEvent::RunStart {
            run_id: "r2".into(),
            agent: None,
            audit: None,
            at: Utc::now(),
        });

        let runs = state.runs();
        assert_eq!(
            runs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["r1", "r2"]
        );
        assert!(runs[0].ended_at.is_some());
        assert!(runs[1].ended_at.is_none());

        let current = state.current_run().unwrap();
        assert_eq!(current.id, "r2");
        assert!(current.ended_at.is_none());
    }
}
