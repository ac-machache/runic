use std::sync::Arc;

use chrono::Utc;
use runic_agent::Llm;
use runic_hook::HookOutcome;
use runic_macros::hook;
use runic_provider::{CompletionRequest, Provider};
use runic_state::{AgentEvent, AgentState};
use runic_types::{ContentBlock, Message, MessageContent, Role};

pub const DEFAULT_SUMMARY_GUIDANCE: &str = "You compress conversation history. Summarize the transcript \
faithfully and densely: goals, decisions, facts, tool results worth keeping, open sessions, and \
the user's constraints or preferences. Third person, no preamble, no commentary — output only \
the summary.";

const SUMMARY_MARKER: &str = "[Conversation summary — earlier context was compacted]";

const CHARS_PER_TOKEN: usize = 4;

#[hook(kind = write, name = "compaction", at = before_model)]
#[derive(Clone)]
pub struct Compaction {
    max_context_tokens: usize,
    keep_recent: usize,
    provider: Arc<dyn Provider>,
    model: String,
    guidance: String,
}

impl Compaction {
    pub fn new(summarizer: Llm) -> Self {
        Self {
            max_context_tokens: 132_000,
            keep_recent: 10,
            provider: summarizer.provider(),
            model: summarizer.config().model.clone(),
            guidance: DEFAULT_SUMMARY_GUIDANCE.to_string(),
        }
    }
    pub fn max_context_tokens(mut self, tokens: usize) -> Self {
        self.max_context_tokens = tokens;
        self
    }
    pub fn keep_recent(mut self, messages: usize) -> Self {
        self.keep_recent = messages;
        self
    }
    pub fn summary_guidance(mut self, guidance: impl Into<String>) -> Self {
        self.guidance = guidance.into();
        self
    }

    async fn hook(&self, state: &mut AgentState, request: &mut CompletionRequest) -> HookOutcome {
        let est_tokens = {
            let msgs = request.messages.as_slice();
            let total_chars: usize = msgs.iter().map(|m| m.content.text_length()).sum();
            let est =
                (total_chars / CHARS_PER_TOKEN).max(state.stats().last_prompt_tokens as usize);
            if est <= self.max_context_tokens || msgs.len() <= self.keep_recent {
                return HookOutcome::Noop;
            }
            est
        };
        let msgs = request.messages.clone();

        let want = msgs.len() - self.keep_recent;
        let Some(split) = clean_boundary(&msgs, want) else {
            tracing::warn!(
                est_tokens,
                "compaction due but no clean tail boundary found — skipped"
            );
            return HookOutcome::Noop;
        };

        let transcript = render(&msgs[..split]);
        let summarize = CompletionRequest {
            model: self.model.clone(),
            messages: vec![Message::user(transcript)],
            tools: vec![],
            max_tokens: 2048,
            temperature: 0.2,
            system: Some(self.guidance.clone()),
            thinking: None,
        };
        let summary = match self.provider.complete(summarize).await {
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

        let run_id = state.current_run_id().unwrap_or("compaction").to_string();
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
        let reason = format!(
            "context ~{est_tokens} tokens > {} max",
            self.max_context_tokens
        );
        let system_prompt = state.system_prompt.clone();
        request.messages = messages.clone();
        state.emit(AgentEvent::StateSnapshot {
            run_id,
            messages,
            system_prompt,
            reason,
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
