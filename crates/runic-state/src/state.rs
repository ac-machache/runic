//! `AgentState` — the agent's working state for one conversation.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use runic_types::Message;
use serde::{Deserialize, Serialize};

use crate::event::AgentEvent;

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

pub trait Emitter: Send + Sync + std::fmt::Debug {
    fn emit(&self, event: AgentEvent);
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

    #[serde(skip, default)]
    current_run_id: Option<String>,

    #[serde(skip, default)]
    pub runtime: RunTimeContext,

    #[serde(skip, default)]
    pub config: serde_json::Map<String, serde_json::Value>,

    #[serde(skip, default)]
    emitters: Vec<Arc<dyn Emitter>>,

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
            current_run_id: None,
            runtime: RunTimeContext::default(),
            config: serde_json::Map::new(),
            emitters: Vec::new(),
            messages: Vec::new(),
        }
    }

    pub fn config(&self, key: &str) -> Option<&serde_json::Value> {
        self.config.get(key)
    }

    pub fn set_emitters(&mut self, emitters: Vec<Arc<dyn Emitter>>) {
        self.emitters = emitters;
    }

    pub fn set_emitter(&mut self, emitter: Option<Arc<dyn Emitter>>) {
        self.emitters = emitter.into_iter().collect();
    }

    pub fn emitters(&self) -> &[Arc<dyn Emitter>] {
        &self.emitters
    }

    pub fn observed(&self) -> bool {
        !self.emitters.is_empty()
    }

    pub fn emit(&mut self, ev: AgentEvent) {
        self.fold(&ev);
        let Some((last, rest)) = self.emitters.split_last() else {
            return;
        };
        for emitter in rest {
            emitter.emit(ev.clone());
        }
        last.emit(ev);
    }

    pub fn fold(&mut self, ev: &AgentEvent) {
        self.stats.fold(ev);
        match ev {
            AgentEvent::RunStarted { run_id, .. } => {
                self.current_run_id = Some(run_id.clone());
            }
            AgentEvent::RunEnd { .. } => {
                self.current_run_id = None;
            }
            AgentEvent::Message { msg, .. } => self.messages.push(msg.clone()),
            AgentEvent::TaskSpawned {
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
            AgentEvent::TaskFinished {
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
            AgentEvent::StateUpdated { key, value, .. } => {
                self.data.insert(key.clone(), value.clone());
            }
            AgentEvent::StateSnapshot {
                messages,
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
            }
            _ => {}
        }
    }

    pub fn update(
        &mut self,
        key: impl Into<String>,
        value: serde_json::Value,
    ) -> Result<(), InvalidStateKey> {
        let key = key.into();
        validate_state_key(&key)?;
        let run_id = self
            .current_run_id
            .clone()
            .unwrap_or_else(|| "update".to_string());
        self.emit(AgentEvent::StateUpdated {
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

    pub fn current_run_id(&self) -> Option<&str> {
        self.current_run_id.as_deref()
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

    pub fn messages_for_provider(&self) -> &[Message] {
        &self.messages
    }

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

pub trait Reader {
    fn messages(&self) -> &[Message];
    fn last_assistant_text(&self) -> Option<String>;
    fn stats(&self) -> &crate::stats::ThreadStats;
    fn tasks(&self) -> &HashMap<String, crate::tasks::TaskRecord>;
    fn open_tasks(&self) -> Vec<crate::tasks::TaskRecord>;
    fn data(&self) -> &serde_json::Map<String, serde_json::Value>;
    fn get(&self, key: &str) -> Option<&serde_json::Value>;
    fn current_run_id(&self) -> Option<&str>;
    fn config(&self, key: &str) -> Option<&serde_json::Value>;
    fn runtime(&self) -> &RunTimeContext;
}

impl Reader for AgentState {
    fn messages(&self) -> &[Message] {
        &self.messages
    }
    fn last_assistant_text(&self) -> Option<String> {
        for msg in self.messages.iter().rev() {
            if msg.role == runic_types::Role::Assistant {
                let text = msg.content.text_content();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
        None
    }
    fn stats(&self) -> &crate::stats::ThreadStats {
        &self.stats
    }
    fn tasks(&self) -> &HashMap<String, crate::tasks::TaskRecord> {
        &self.tasks
    }
    fn open_tasks(&self) -> Vec<crate::tasks::TaskRecord> {
        let mut open: Vec<_> = self
            .tasks
            .values()
            .filter(|task| task.status == crate::tasks::TaskStatus::Running)
            .cloned()
            .collect();
        open.sort_by(|a, b| a.spawned_at.cmp(&b.spawned_at));
        open
    }
    fn data(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.data
    }
    fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.data.get(key)
    }
    fn current_run_id(&self) -> Option<&str> {
        self.current_run_id.as_deref()
    }
    fn config(&self, key: &str) -> Option<&serde_json::Value> {
        self.config.get(key)
    }
    fn runtime(&self) -> &RunTimeContext {
        &self.runtime
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookLifecycle;

    fn message(text: &str, user: bool) -> AgentEvent {
        let msg = if user {
            Message::user(text)
        } else {
            Message::assistant(text)
        };
        AgentEvent::Message {
            run_id: "r".into(),
            msg,
            at: Utc::now(),
        }
    }

    fn run_started(run_id: &str) -> AgentEvent {
        AgentEvent::RunStarted {
            run_id: run_id.into(),
            agent: None,
            audit: None,
            at: Utc::now(),
        }
    }

    #[test]
    fn emit_folds_messages_and_skips_non_messages() {
        let mut state = AgentState::new("u", "s", "sys");
        state.emit(run_started("r"));
        state.emit(message("hello", true));
        state.emit(message("hi there", false));

        assert_eq!(state.messages_for_provider().len(), 2);
        assert_eq!(state.current_run_id(), Some("r"));
    }

    #[test]
    fn a_hook_event_leaves_the_message_view_untouched() {
        let mut state = AgentState::new("u", "s", "sys");
        state.emit(message("hello", true));
        state.emit(AgentEvent::HookFired {
            run_id: "r".into(),
            hook: "guard".into(),
            lifecycle: HookLifecycle::BeforeTool,
            hook_kind: "write".into(),
            outcome: "cancel".into(),
            note: Some("blocked".into()),
            at: Utc::now(),
        });

        assert_eq!(state.messages_for_provider().len(), 1);
    }

    #[test]
    fn state_snapshot_replaces_the_message_view() {
        let mut state = AgentState::new("u", "s", "sys");
        state.emit(message("old one", true));
        state.emit(message("old two", false));
        state.emit(AgentEvent::StateSnapshot {
            run_id: "r".into(),
            messages: vec![Message::user("compacted")],
            system_prompt: "sys".into(),
            reason: "compaction".into(),
            stats: None,
            open_tasks: None,
            data: None,
            at: Utc::now(),
        });
        state.emit(message("after", false));

        let msgs = state.messages_for_provider();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content.text_content(), "compacted");
        assert_eq!(msgs[1].content.text_content(), "after");
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
    }

    #[test]
    fn current_run_id_tracks_the_in_flight_run() {
        let mut state = AgentState::new("u", "s", "sys");
        state.emit(run_started("r1"));
        state.emit(AgentEvent::RunEnd {
            run_id: "r1".into(),
            status: crate::event::RunEndStatus::Completed,
            outcome: crate::event::RunOutcome::default(),
            at: Utc::now(),
        });
        assert_eq!(state.current_run_id(), None);

        state.emit(run_started("r2"));
        assert_eq!(state.current_run_id(), Some("r2"));
    }
}
