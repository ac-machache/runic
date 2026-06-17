//! `TitleReflector` — names a thread after its first exchange.
//!
//! coral's `CoralTitleReflectorMiddleware` generates a short thread title
//! with a cheap model once a conversation has a real exchange, then writes
//! it to the thread row. runic's version is an `after_agent` hook holding a
//! provider (point a Haiku at it) and a [`TitleSink`] the host implements
//! to persist the title wherever it lives (DB, thread metadata, …).
//!
//! Gating is by first turn: it fires once, when the run that just finished
//! is the thread's first user turn. Later turns are skipped. (coral also
//! refines the title a second time — left as a follow-up.)

use std::sync::Arc;

use async_trait::async_trait;
use runic_agent_core::{AgentState, Hook, HookOutcome};
use runic_message_types::{ContentBlock, Message, Role};
use runic_provider_core::Provider;

/// Where a generated title goes. The hook is storage-agnostic; the host
/// wires a concrete sink (Postgres thread row, in-memory map, log, …).
#[async_trait]
pub trait TitleSink: Send + Sync {
    async fn set_title(&self, session_id: &str, title: &str);
}

/// Default sink: emit the title on the tracing stream. Useful for a REPL /
/// dev runs before a real persistence sink is wired.
pub struct LoggingTitleSink;

#[async_trait]
impl TitleSink for LoggingTitleSink {
    async fn set_title(&self, session_id: &str, title: &str) {
        tracing::info!(session_id, title, "thread title generated");
    }
}

const TITLE_SYSTEM: &str =
    "Tu génères des titres de conversation courts et factuels (max 6 mots), \
     sans guillemets ni ponctuation finale. Réponds UNIQUEMENT par le titre.";

pub struct TitleReflector {
    provider: Arc<dyn Provider>,
    sink: Arc<dyn TitleSink>,
}

impl TitleReflector {
    pub fn new(provider: Arc<dyn Provider>, sink: Arc<dyn TitleSink>) -> Self {
        Self { provider, sink }
    }
}

/// First text block of a message of the given role.
fn first_text_of_role(msgs: &[Message], role: Role) -> Option<&str> {
    msgs.iter().find(|m| m.role == role).and_then(|m| {
        m.content.iter().find_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
    })
}

/// Generate the title once the thread reaches this many user turns — enough
/// exchange to title it meaningfully, without waiting forever.
const TITLE_AFTER_USER_TURNS: usize = 4;

/// True on exactly the Nth user turn (`TITLE_AFTER_USER_TURNS`), so the title
/// is generated once. Earlier turns are too thin; later turns already have a
/// title.
fn is_title_turn(msgs: &[Message]) -> bool {
    msgs.iter().filter(|m| m.role == Role::User).count() == TITLE_AFTER_USER_TURNS
}

/// Build the title-generation prompt from the first exchange. Returns
/// `None` when there's no user text to title.
fn title_prompt(msgs: &[Message]) -> Option<String> {
    let user = first_text_of_role(msgs, Role::User)?;
    let mut prompt = format!("Utilisateur : {user}");
    if let Some(assistant) = first_text_of_role(msgs, Role::Assistant) {
        prompt.push_str(&format!("\nAssistant : {assistant}"));
    }
    Some(prompt)
}

/// Trim model noise: drop surrounding quotes/whitespace and any trailing
/// period so the stored title is clean.
fn clean_title(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| c == '"' || c == '«' || c == '»')
        .trim()
        .trim_end_matches('.')
        .to_string()
}

#[async_trait::async_trait]
impl Hook for TitleReflector {
    fn name(&self) -> &'static str {
        "title_reflector"
    }

    async fn after_agent(&self, state: &mut AgentState) -> HookOutcome {
        let msgs = state.messages_for_provider();
        if !is_title_turn(&msgs) {
            return HookOutcome::Continue;
        }
        let Some(prompt) = title_prompt(&msgs) else {
            return HookOutcome::Continue;
        };

        match self.provider.complete_simple(&prompt, TITLE_SYSTEM).await {
            Ok(raw) => {
                let title = clean_title(&raw);
                if !title.is_empty() {
                    self.sink.set_title(&state.session_id, &title).await;
                }
            }
            Err(e) => {
                // Fail-silent: a missing title never breaks a turn.
                tracing::warn!(error = %e, "title generation failed");
            }
        }
        HookOutcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use runic_message_types::StreamEvent;
    use runic_provider_core::{EventStream, ProviderError};
    use std::sync::Mutex;

    // ── a tiny scripted provider that emits one text delta ──────────────
    struct FakeProvider {
        reply: String,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[runic_message_types::ToolDefinition],
            _system: &str,
            _resume: Option<&str>,
        ) -> Result<EventStream, ProviderError> {
            let ev = Ok(StreamEvent::TextDelta(self.reply.clone()));
            Ok(Box::pin(stream::iter(vec![ev])))
        }
        fn name(&self) -> &str {
            "fake"
        }
        fn model(&self) -> String {
            "fake-1".into()
        }
        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(FakeProvider {
                reply: self.reply.clone(),
            })
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        titles: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl TitleSink for RecordingSink {
        async fn set_title(&self, session_id: &str, title: &str) {
            self.titles
                .lock()
                .unwrap()
                .push((session_id.to_string(), title.to_string()));
        }
    }

    fn state_with(msgs: Vec<Message>) -> AgentState {
        let mut s = AgentState::new("sess-1", "");
        for m in msgs {
            s.events.push(runic_agent_core::SessionEvent::Message {
                run_id: "r1".into(),
                msg: m,
                at: chrono::Utc::now(),
            });
        }
        s
    }

    /// `n` user+assistant exchanges.
    fn turns(n: usize) -> Vec<Message> {
        let mut v = Vec::new();
        for i in 0..n {
            v.push(Message::user(&format!("u{i}")));
            v.push(Message::assistant_text(&format!("a{i}")));
        }
        v
    }

    #[test]
    fn is_title_turn_fires_only_at_threshold() {
        assert!(!is_title_turn(&turns(3)));
        assert!(is_title_turn(&turns(TITLE_AFTER_USER_TURNS))); // == 4
        assert!(!is_title_turn(&turns(5)));
    }

    #[test]
    fn clean_title_strips_quotes_and_period() {
        assert_eq!(clean_title("  \"Visite Dupont.\"  "), "Visite Dupont");
        assert_eq!(clean_title("« Préparation visite »"), "Préparation visite");
    }

    #[tokio::test]
    async fn generates_title_on_the_threshold_turn() {
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider {
            reply: "\"Visite Dupont\"".into(),
        });
        let sink = Arc::new(RecordingSink::default());
        let hook = TitleReflector::new(provider, sink.clone());

        let mut state = state_with(turns(TITLE_AFTER_USER_TURNS)); // 4 user turns
        hook.after_agent(&mut state).await;

        let titles = sink.titles.lock().unwrap();
        assert_eq!(titles.len(), 1);
        assert_eq!(titles[0].0, "sess-1");
        assert_eq!(titles[0].1, "Visite Dupont");
    }

    #[tokio::test]
    async fn skips_before_threshold() {
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider { reply: "X".into() });
        let sink = Arc::new(RecordingSink::default());
        let hook = TitleReflector::new(provider, sink.clone());

        let mut state = state_with(turns(2)); // only 2 user turns → too early
        hook.after_agent(&mut state).await;

        assert!(sink.titles.lock().unwrap().is_empty());
    }
}
