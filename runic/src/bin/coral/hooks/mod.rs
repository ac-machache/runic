//! Tenant-id injection — the runic equivalent of coral's client-side
//! `bound_params`. A `WriteHook` that, before an MCP toolbox tool runs,
//! overwrites the scoped ids in the call from the run's config, so the model
//! never supplies them (and the value never leaks into the prompt).
//!
//! Values are read from `state.config` (the per-run open map), populated per
//! request — the multi-user seam. Scope each agent's hook to its toolset:
//!   Maia → `mcp__coral__` + ["user_id"]; crm-expert → `mcp__crm-expert__` +
//!   ["user_id", "org_id"]; product-expert (ephy) → none.

use async_trait::async_trait;
use runic_hook::{HookOutcome, WriteHook};
use runic_provider::{CompletionRequest, Provider};
use runic_state::{AgentState, SessionEvent};
use runic_substrate::SessionStore;
use runic_types::{Message, Role, ToolCall};
use std::sync::Arc;

const TITLE_MAX_CHARS: usize = 60;
const TITLE_CONTEXT_CHARS: usize = 4000;

pub struct InjectIds {
    prefix: String,
    ids: Vec<String>,
}

impl InjectIds {
    pub fn new(
        prefix: impl Into<String>,
        ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            ids: ids.into_iter().map(Into::into).collect(),
        }
    }
}

#[async_trait]
impl WriteHook for InjectIds {
    fn name(&self) -> &str {
        "inject-ids"
    }

    async fn before_tool(&self, state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        if !call.name.starts_with(&self.prefix) {
            return HookOutcome::Continue;
        }
        if !call.input.is_object() {
            call.input = serde_json::Value::Object(serde_json::Map::new());
        }
        let obj = call.input.as_object_mut().expect("input is an object");
        for id in &self.ids {
            if let Some(value) = state.config.get(id).cloned() {
                obj.insert(id.clone(), value);
            } else {
                tracing::warn!(id = %id, tool = %call.name, "id not in run config — tool call will be unscoped");
            }
        }
        HookOutcome::Continue
    }
}

/// Forces composio's `entity_id` to the run's `user_id` (from `state.config`),
/// so the model acts only as its own user's connected accounts — never one it
/// names itself. Composio is the orchestrator's tool, so this rides on Maia.
pub struct ComposioEntity;

#[async_trait]
impl WriteHook for ComposioEntity {
    fn name(&self) -> &str {
        "composio-entity"
    }

    async fn before_tool(&self, state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        if call.name != "composio" {
            return HookOutcome::Continue;
        }
        let Some(user_id) = state.config.get("user_id").cloned() else {
            tracing::warn!("user_id not in run config — composio call will use the default entity");
            return HookOutcome::Continue;
        };
        if !call.input.is_object() {
            call.input = serde_json::Value::Object(serde_json::Map::new());
        }
        call.input
            .as_object_mut()
            .expect("input is an object")
            .insert("entity_id".into(), user_id);
        HookOutcome::Continue
    }
}

/// Derives a thread title after the first successful run, persists it to
/// session metadata, then mirrors it into the warm `AgentState`.
pub struct ThreadTitle {
    store: Arc<dyn SessionStore>,
    provider: Arc<dyn Provider>,
    model: String,
}

impl ThreadTitle {
    pub fn new(
        store: Arc<dyn SessionStore>,
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            store,
            provider,
            model: model.into(),
        }
    }
}

#[async_trait]
impl WriteHook for ThreadTitle {
    fn name(&self) -> &str {
        "thread-title"
    }

    async fn after_agent(&self, state: &mut AgentState) -> HookOutcome {
        let user_turns = user_turn_count(state);
        if !should_generate_title(user_turns) {
            return HookOutcome::Continue;
        };
        let title = match self.generate_title(state).await {
            Some(title) => title,
            None => match first_user_title(state) {
                Some(title) => title,
                None => return HookOutcome::Continue,
            },
        };
        match self
            .store
            .set_label(&state.user_id, &state.session_id, Some(&title))
            .await
        {
            Ok(()) => state.label = Some(title),
            Err(e) => tracing::warn!(error = %e, "failed to persist thread title"),
        }
        HookOutcome::Continue
    }
}

impl ThreadTitle {
    async fn generate_title(&self, state: &AgentState) -> Option<String> {
        let transcript = title_transcript(state)?;
        let prompt = format!(
            "Generate a concise chat title for this conversation.\n\
             Rules:\n\
             - Return only the title.\n\
             - No quotes, markdown, punctuation-only titles, or trailing period.\n\
             - 3 to 7 words when possible.\n\
             - Keep it under {TITLE_MAX_CHARS} characters.\n\n\
             Conversation:\n{transcript}"
        );
        let response = self
            .provider
            .complete(CompletionRequest {
                model: self.model.clone(),
                messages: vec![Message::user(prompt)],
                tools: Vec::new(),
                max_tokens: 32,
                temperature: 0.2,
                system: Some("You write short, plain conversation titles.".into()),
                thinking: None,
            })
            .await;

        match response {
            Ok(response) => title_from_text(&response.text()),
            Err(e) => {
                tracing::warn!(error = %e, "failed to generate AI thread title");
                None
            }
        }
    }
}

fn should_generate_title(user_turns: usize) -> bool {
    user_turns == 2 || (user_turns >= 16 && user_turns.is_power_of_two())
}

fn user_turn_count(state: &AgentState) -> usize {
    state
        .events
        .iter()
        .filter_map(message_event)
        .filter(|msg| msg.role == Role::User && !msg.content.text_content().trim().is_empty())
        .count()
}

fn first_user_title(state: &AgentState) -> Option<String> {
    state.events.iter().find_map(|event| {
        let msg = message_event(event)?;
        (msg.role == Role::User).then(|| title_from_text(&msg.content.text_content()))?
    })
}

fn title_transcript(state: &AgentState) -> Option<String> {
    let mut lines = Vec::new();
    for msg in state.events.iter().filter_map(message_event) {
        let text = msg.content.text_content();
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => continue,
        };
        lines.push(format!("{role}: {text}"));
    }
    let transcript = lines.join("\n");
    if transcript.is_empty() {
        return None;
    }
    Some(truncate_chars(&transcript, TITLE_CONTEXT_CHARS))
}

fn message_event(event: &SessionEvent) -> Option<&runic_types::Message> {
    let SessionEvent::Message { msg, .. } = event else {
        return None;
    };
    Some(msg)
}

fn title_from_text(text: &str) -> Option<String> {
    let first_line = text.lines().find(|line| !line.trim().is_empty())?;
    let without_prefix = first_line
        .trim()
        .strip_prefix("Title:")
        .unwrap_or(first_line)
        .trim();
    let collapsed = without_prefix
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let title = collapsed
        .trim_end_matches('.')
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '#'))
        .trim_end_matches('.')
        .trim();
    if title.is_empty() {
        return None;
    }
    if title.chars().count() <= TITLE_MAX_CHARS {
        return Some(title.to_string());
    }
    Some(format!("{}...", truncate_chars(title, TITLE_MAX_CHARS - 3)))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use runic_provider::{CompletionResponse, ProviderError};
    use runic_state::SessionEvent;
    use runic_substrate::MemorySessionStore;
    use runic_types::{ContentBlock, Message, StopReason, TokenUsage};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StaticTitleProvider {
        title: String,
        calls: AtomicUsize,
    }

    impl StaticTitleProvider {
        fn new(title: &str) -> Self {
            Self {
                title: title.into(),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Provider for StaticTitleProvider {
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            assert_eq!(request.model, "title-model");
            assert!(request.tools.is_empty());
            assert_eq!(request.max_tokens, 32);
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: self.title.clone(),
                    provider_metadata: None,
                }],
                stop_reason: StopReason::EndTurn,
                tool_calls: Vec::new(),
                usage: TokenUsage::default(),
            })
        }
    }

    fn push_user(state: &mut AgentState, n: usize) {
        state.push_event(SessionEvent::Message {
            run_id: format!("r{n}"),
            msg: Message::user(format!("User request {n}")),
            at: Utc::now(),
        });
    }

    #[test]
    fn title_collapses_and_truncates() {
        let title = title_from_text(
            "   \"This   is a very long title that should be trimmed down to the configured limit cleanly\"   ",
        )
        .unwrap();

        assert_eq!(title.chars().count(), TITLE_MAX_CHARS);
        assert!(title.ends_with("..."));
        assert!(!title.contains("  "));
    }

    #[tokio::test]
    async fn thread_title_generates_at_second_user_turn() {
        let store = Arc::new(MemorySessionStore::new());
        let provider = Arc::new(StaticTitleProvider::new("\"Launch planning\"."));
        let hook = ThreadTitle::new(store.clone(), provider.clone(), "title-model");
        let mut state = AgentState::new("tenant", "thread", "system");
        push_user(&mut state, 1);

        let _ = hook.after_agent(&mut state).await;
        assert_eq!(state.label, None);
        assert_eq!(provider.calls(), 0);

        push_user(&mut state, 2);

        assert!(matches!(
            hook.after_agent(&mut state).await,
            HookOutcome::Continue
        ));

        assert_eq!(state.label.as_deref(), Some("Launch planning"));
        assert_eq!(provider.calls(), 1);
        let meta = store
            .session_meta("tenant", "thread")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.label.as_deref(), state.label.as_deref());
    }

    #[tokio::test]
    async fn thread_title_updates_again_at_scheduled_power_turns() {
        let store = Arc::new(MemorySessionStore::new());
        let provider = Arc::new(StaticTitleProvider::new("Updated roadmap"));
        let hook = ThreadTitle::new(store.clone(), provider.clone(), "title-model");
        let mut state = AgentState::new("tenant", "thread", "system");
        state.label = Some("Early title".into());
        for n in 1..=16 {
            push_user(&mut state, n);
        }

        let _ = hook.after_agent(&mut state).await;

        assert_eq!(state.label.as_deref(), Some("Updated roadmap"));
        assert_eq!(provider.calls(), 1);
        let meta = store
            .session_meta("tenant", "thread")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.label.as_deref(), Some("Updated roadmap"));
    }

    #[tokio::test]
    async fn thread_title_skips_unscheduled_turn_counts() {
        let store = Arc::new(MemorySessionStore::new());
        let provider = Arc::new(StaticTitleProvider::new("Should not run"));
        let hook = ThreadTitle::new(store.clone(), provider.clone(), "title-model");
        let mut state = AgentState::new("tenant", "thread", "system");
        state.label = Some("Existing".into());
        for n in 1..=4 {
            push_user(&mut state, n);
        }

        let _ = hook.after_agent(&mut state).await;

        assert_eq!(state.label.as_deref(), Some("Existing"));
        assert_eq!(provider.calls(), 0);
    }
}
