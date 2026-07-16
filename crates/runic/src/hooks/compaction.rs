use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_provider::{CompletionRequest, Provider};
use runic_state::{AgentState, SessionEvent};
use runic_types::{ContentBlock, Message, MessageContent, Role};

pub const DEFAULT_SUMMARY_GUIDANCE: &str = "You compress conversation history. Summarize the transcript \
faithfully and densely: goals, decisions, facts, tool results worth keeping, open threads, and \
the user's constraints or preferences. Third person, no preamble, no commentary — output only \
the summary.";

const SUMMARY_MARKER: &str = "[Conversation summary — earlier context was compacted]";

const CHARS_PER_TOKEN: usize = 4;

#[derive(Clone)]
pub struct Compaction {
    pub max_context_tokens: usize,
    pub keep_recent: usize,
    pub provider: Option<Arc<dyn Provider>>,
    pub model: Option<String>,
    pub summary_guidance: Option<String>,
}

impl Default for Compaction {
    fn default() -> Self {
        Self {
            max_context_tokens: 132_000,
            keep_recent: 10,
            provider: None,
            model: None,
            summary_guidance: None,
        }
    }
}

impl Compaction {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn max_context_tokens(mut self, tokens: usize) -> Self {
        self.max_context_tokens = tokens;
        self
    }
    pub fn keep_recent(mut self, messages: usize) -> Self {
        self.keep_recent = messages;
        self
    }
    pub fn summarizer(mut self, provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        self.provider = Some(provider);
        self.model = Some(model.into());
        self
    }
    pub fn summary_guidance(mut self, guidance: impl Into<String>) -> Self {
        self.summary_guidance = Some(guidance.into());
        self
    }
}

pub(crate) struct CompactionHook {
    max_context_tokens: usize,
    keep_recent: usize,
    provider: Arc<dyn Provider>,
    model: String,
    guidance: String,
}

impl CompactionHook {
    pub(crate) fn new(
        cfg: &Compaction,
        agent_provider: Arc<dyn Provider>,
        agent_model: &str,
    ) -> Self {
        Self {
            max_context_tokens: cfg.max_context_tokens,
            keep_recent: cfg.keep_recent,
            provider: cfg.provider.clone().unwrap_or(agent_provider),
            model: cfg.model.clone().unwrap_or_else(|| agent_model.to_string()),
            guidance: cfg
                .summary_guidance
                .clone()
                .unwrap_or_else(|| DEFAULT_SUMMARY_GUIDANCE.to_string()),
        }
    }
}

#[async_trait]
impl WriteHook for CompactionHook {
    fn name(&self) -> &str {
        "compaction"
    }

    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::BeforeModel]
    }

    async fn before_model(&self, state: &mut AgentState) -> HookOutcome {
        let est_tokens = {
            let msgs = state.messages_for_provider();
            let total_chars: usize = msgs.iter().map(|m| m.content.text_length()).sum();
            let est =
                (total_chars / CHARS_PER_TOKEN).max(state.stats().last_prompt_tokens as usize);
            if est <= self.max_context_tokens || msgs.len() <= self.keep_recent {
                return HookOutcome::Noop;
            }
            est
        };
        let msgs = state.messages_for_provider().to_vec();

        let want = msgs.len() - self.keep_recent;
        let Some(split) = clean_boundary(&msgs, want) else {
            tracing::warn!(
                est_tokens,
                "compaction due but no clean tail boundary found — skipped"
            );
            return HookOutcome::Noop;
        };

        let transcript = render(&msgs[..split]);
        let request = CompletionRequest {
            model: self.model.clone(),
            messages: vec![Message::user(transcript)],
            tools: vec![],
            max_tokens: 2048,
            temperature: 0.2,
            system: Some(self.guidance.clone()),
            thinking: None,
        };
        let summary = match self.provider.complete(request).await {
            Ok(response) => response.text(),
            Err(e) => {
                tracing::warn!(error = %e, "compaction summarizer failed — skipped");
                return HookOutcome::Noop;
            }
        };
        if summary.trim().is_empty() {
            tracing::warn!("compaction summarizer returned nothing — skipped");
            return HookOutcome::Noop;
        }

        let mut messages = Vec::with_capacity(msgs.len() - split + 1);
        messages.push(Message::assistant(format!("{SUMMARY_MARKER}\n{summary}")));
        messages.extend_from_slice(&msgs[split..]);

        let run_id = state
            .current_run()
            .map(|r| r.id.clone())
            .unwrap_or_else(|| "compaction".to_string());
        let folded = split;
        let stats = state.stats().clone();
        let open_tasks = state.open_tasks();
        let open_ids: std::collections::HashSet<&str> =
            open_tasks.iter().map(|t| t.task_id.as_str()).collect();
        let mut data = state.data().clone();
        data.retain(|key, _| {
            key.strip_prefix(super::task_reminder::NOTIFIED_KEY_PREFIX)
                .is_none_or(|id| open_ids.contains(id))
        });
        state.push_event(SessionEvent::StateSnapshot {
            run_id,
            messages,
            system_prompt: state.system_prompt.clone(),
            reason: format!(
                "context ~{est_tokens} tokens > {} max",
                self.max_context_tokens
            ),
            stats: Some(Box::new(stats)),
            open_tasks: Some(open_tasks),
            data: Some(data),
            at: Utc::now(),
        });
        tracing::info!(
            folded_messages = folded,
            kept_messages = msgs.len() - split,
            est_tokens,
            "context compacted"
        );
        HookOutcome::Continue
    }
}

fn clean_boundary(msgs: &[Message], want: usize) -> Option<usize> {
    (want..msgs.len()).find(|&i| is_plain_user(&msgs[i]))
}

fn is_plain_user(msg: &Message) -> bool {
    if msg.role != Role::User {
        return false;
    }
    match &msg.content {
        MessageContent::Text(_) => true,
        MessageContent::Blocks(blocks) => !blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. })),
    }
}

fn render(msgs: &[Message]) -> String {
    let mut out = String::new();
    for msg in msgs {
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => continue,
        };
        let text = msg.content.text_content();
        if !text.trim().is_empty() {
            out.push_str(role);
            out.push_str(": ");
            out.push_str(&text);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_result_msg() -> Message {
        Message::user_with_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            tool_name: "rec".into(),
            content: "out".into(),
            is_error: false,
            provenance: Vec::new(),
        }])
    }

    #[test]
    fn boundary_skips_tool_result_carriers() {
        let msgs = vec![
            Message::user("one"),
            Message::assistant("calling tool"),
            tool_result_msg(),
            Message::assistant("done"),
            Message::user("two"),
            Message::assistant("reply"),
        ];
        assert_eq!(clean_boundary(&msgs, 2), Some(4));
    }

    #[test]
    fn boundary_lands_on_plain_user() {
        let msgs = vec![
            Message::user("one"),
            Message::assistant("a"),
            Message::user("two"),
            Message::assistant("b"),
        ];
        assert_eq!(clean_boundary(&msgs, 2), Some(2));
    }

    #[test]
    fn no_boundary_when_tail_is_all_tool_traffic() {
        let msgs = vec![
            Message::user("one"),
            Message::assistant("calling"),
            tool_result_msg(),
            Message::assistant("still going"),
        ];
        assert_eq!(clean_boundary(&msgs, 1), None);
    }
}
