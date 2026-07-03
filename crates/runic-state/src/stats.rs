use std::collections::HashMap;

use runic_types::{ContentBlock, MessageContent};
use serde::{Deserialize, Serialize};

use crate::event::SessionEvent;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThreadStats {
    pub runs: u64,
    pub turns: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tool_calls: u64,
    pub tool_calls: HashMap<String, u64>,
}

impl ThreadStats {
    pub fn fold(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::RunEnd { outcome, .. } => {
                self.runs += 1;
                self.turns += outcome.total_turns as u64;
                self.input_tokens += outcome.usage.input_tokens;
                self.output_tokens += outcome.usage.output_tokens;
            }
            SessionEvent::Message { msg, .. } => {
                let MessageContent::Blocks(blocks) = &msg.content else {
                    return;
                };
                for block in blocks {
                    if let ContentBlock::ToolResult { tool_name, .. } = block {
                        *self.tool_calls.entry(tool_name.clone()).or_insert(0) += 1;
                        self.total_tool_calls += 1;
                    }
                }
            }
            SessionEvent::StateSnapshot {
                stats: Some(stats), ..
            } => {
                *self = stats.clone();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RunOutcome;
    use chrono::Utc;
    use runic_types::{Message, TokenUsage};

    fn tool_result(tool: &str) -> SessionEvent {
        SessionEvent::Message {
            run_id: "r1".into(),
            msg: Message::user_with_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "t".into(),
                tool_name: tool.into(),
                content: "ok".into(),
                is_error: false,
            }]),
            at: Utc::now(),
        }
    }

    fn run_end(turns: u32, input: u64, output: u64) -> SessionEvent {
        SessionEvent::RunEnd {
            run_id: "r1".into(),
            outcome: RunOutcome {
                total_turns: turns,
                stop_reason: None,
                usage: TokenUsage {
                    input_tokens: input,
                    output_tokens: output,
                },
                structured: None,
            },
            at: Utc::now(),
        }
    }

    #[test]
    fn folds_runs_turns_tokens_and_tool_calls() {
        let mut stats = ThreadStats::default();
        stats.fold(&tool_result("payment"));
        stats.fold(&tool_result("payment"));
        stats.fold(&tool_result("search"));
        stats.fold(&run_end(3, 100, 40));
        stats.fold(&run_end(2, 50, 10));

        assert_eq!(stats.runs, 2);
        assert_eq!(stats.turns, 5);
        assert_eq!(stats.input_tokens, 150);
        assert_eq!(stats.output_tokens, 50);
        assert_eq!(stats.total_tool_calls, 3);
        assert_eq!(stats.tool_calls["payment"], 2);
        assert_eq!(stats.tool_calls["search"], 1);
    }

    #[test]
    fn a_snapshot_with_stats_is_authoritative() {
        let mut stats = ThreadStats::default();
        stats.fold(&tool_result("payment"));

        let rolled = ThreadStats {
            runs: 9,
            total_tool_calls: 42,
            ..Default::default()
        };
        stats.fold(&SessionEvent::StateSnapshot {
            run_id: "r1".into(),
            messages: vec![],
            system_prompt: "sys".into(),
            reason: "compaction".into(),
            stats: Some(rolled.clone()),
            at: Utc::now(),
        });
        assert_eq!(stats, rolled);
    }

    #[test]
    fn a_stats_less_snapshot_keeps_the_folded_stats() {
        let mut stats = ThreadStats::default();
        stats.fold(&tool_result("payment"));

        stats.fold(&SessionEvent::StateSnapshot {
            run_id: "r1".into(),
            messages: vec![],
            system_prompt: "sys".into(),
            reason: "old row".into(),
            stats: None,
            at: Utc::now(),
        });
        assert_eq!(stats.total_tool_calls, 1);
    }
}
