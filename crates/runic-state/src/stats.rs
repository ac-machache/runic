use std::collections::HashMap;

use runic_types::TokenUsage;
use serde::{Deserialize, Serialize};

use crate::event::{AgentEvent, DelegationStatus, RunEndStatus, ToolStatus};

pub const MAX_TRACKED_TOOLS: usize = 64;
pub const MAX_TRACKED_MODELS: usize = 16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolStat {
    pub calls: u64,
    pub errors: u64,
    pub total_duration_ms: u64,
    pub max_duration_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionStats {
    pub runs: u64,
    pub errored_runs: u64,
    pub cancelled_runs: u64,
    pub turns: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub model_ms: u64,
    pub last_prompt_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,
    pub total_tool_calls: u64,
    pub tools: HashMap<String, ToolStat>,
    pub other_tools: ToolStat,
    pub tokens_by_model: HashMap<String, TokenUsage>,
    pub other_models: TokenUsage,
    pub delegations: u64,
    pub delegation_errors: u64,
    pub delegated_usage: TokenUsage,
    pub tasks_spawned: u64,
    pub tasks_finished: u64,
    pub tasks_failed: u64,
}

impl SessionStats {
    pub fn fold(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::RunStarted { .. } => self.runs += 1,
            AgentEvent::RunEnd { status, .. } => match status {
                RunEndStatus::Completed => {}
                RunEndStatus::Failed(_) => self.errored_runs += 1,
                RunEndStatus::Cancelled => self.cancelled_runs += 1,
            },
            AgentEvent::TurnEnd {
                model,
                usage,
                model_ms,
                ..
            } => {
                self.turns += 1;
                self.input_tokens += usage.input_tokens;
                self.output_tokens += usage.output_tokens;
                self.cache_read_tokens += usage.cache_read_tokens;
                self.cache_write_tokens += usage.cache_write_tokens;
                self.model_ms += model_ms;
                self.last_prompt_tokens = usage.input_tokens;
                self.last_model = Some(model.clone());
                let slot = if self.tokens_by_model.contains_key(model)
                    || self.tokens_by_model.len() < MAX_TRACKED_MODELS
                {
                    self.tokens_by_model.entry(model.clone()).or_default()
                } else {
                    &mut self.other_models
                };
                slot.add(usage);
            }
            AgentEvent::ToolFinished {
                tool,
                status,
                duration_ms,
                ..
            } => {
                self.total_tool_calls += 1;
                let stat = if self.tools.contains_key(tool) || self.tools.len() < MAX_TRACKED_TOOLS
                {
                    self.tools.entry(tool.clone()).or_default()
                } else {
                    &mut self.other_tools
                };
                stat.calls += 1;
                stat.total_duration_ms += duration_ms;
                stat.max_duration_ms = stat.max_duration_ms.max(*duration_ms);
                if !matches!(status, ToolStatus::Ok | ToolStatus::Substituted) {
                    stat.errors += 1;
                }
            }
            AgentEvent::DelegationFinished { status, usage, .. } => {
                self.delegations += 1;
                if matches!(status, DelegationStatus::Failed(_)) {
                    self.delegation_errors += 1;
                }
                self.delegated_usage.add(usage);
            }
            AgentEvent::TaskSpawned { .. } => self.tasks_spawned += 1,
            AgentEvent::TaskFinished { status, .. } => match status {
                crate::tasks::TaskStatus::Completed => self.tasks_finished += 1,
                crate::tasks::TaskStatus::Failed | crate::tasks::TaskStatus::Cancelled => {
                    self.tasks_failed += 1
                }
                crate::tasks::TaskStatus::Running => {}
            },
            AgentEvent::StateSnapshot {
                stats: Some(stats), ..
            } => {
                *self = (**stats).clone();
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
    use runic_types::TokenUsage;

    fn tool_result(tool: &str) -> AgentEvent {
        tool_finished(tool, ToolStatus::Ok, 10)
    }

    fn tool_finished(tool: &str, status: ToolStatus, duration_ms: u64) -> AgentEvent {
        AgentEvent::ToolFinished {
            run_id: "r1".into(),
            turn: 1,
            call_id: "c".into(),
            tool: tool.into(),
            status,
            result: serde_json::Value::Null,
            provenance: Vec::new(),
            duration_ms,
            at: Utc::now(),
        }
    }

    fn run_start() -> AgentEvent {
        AgentEvent::RunStarted {
            run_id: "r1".into(),
            agent: None,
            audit: None,
            at: Utc::now(),
        }
    }

    fn run_end(status: RunEndStatus) -> AgentEvent {
        AgentEvent::RunEnd {
            run_id: "r1".into(),
            status,
            outcome: RunOutcome::default(),
            at: Utc::now(),
        }
    }

    fn turn_end(model: &str, input: u64, output: u64, cache_read: u64, ms: u64) -> AgentEvent {
        AgentEvent::TurnEnd {
            run_id: "r1".into(),
            turn: 1,
            model: model.into(),
            usage: TokenUsage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                ..Default::default()
            },
            model_ms: ms,
            stop_reason: String::new(),
            at: Utc::now(),
        }
    }

    #[test]
    fn runs_count_attempts_and_outcomes_separately() {
        let mut stats = SessionStats::default();
        stats.fold(&run_start());
        stats.fold(&run_end(RunEndStatus::Completed));
        stats.fold(&run_start());
        stats.fold(&run_end(RunEndStatus::Failed("boom".into())));
        stats.fold(&run_start());
        stats.fold(&run_end(RunEndStatus::Cancelled));
        stats.fold(&run_start());

        assert_eq!(stats.runs, 4);
        assert_eq!(stats.errored_runs, 1);
        assert_eq!(stats.cancelled_runs, 1);
    }

    #[test]
    fn turns_feed_tokens_latency_by_model_and_the_prompt_gauge() {
        let mut stats = SessionStats::default();
        stats.fold(&turn_end("m1", 100, 40, 20, 900));
        stats.fold(&turn_end("m2", 50, 10, 0, 100));

        assert_eq!(stats.turns, 2);
        assert_eq!(stats.input_tokens, 150);
        assert_eq!(stats.output_tokens, 50);
        assert_eq!(stats.cache_read_tokens, 20);
        assert_eq!(stats.model_ms, 1000);
        assert_eq!(stats.tokens_by_model["m1"].input_tokens, 100);
        assert_eq!(stats.tokens_by_model["m2"].input_tokens, 50);
        assert_eq!(
            stats.last_prompt_tokens, 50,
            "the gauge is the LAST turn's prompt size, not a sum"
        );

        stats.fold(&turn_end("m1", 500, 1, 300, 10));
        assert_eq!(
            stats.last_prompt_tokens, 500,
            "input_tokens is the whole prompt; cache fields are subsets"
        );
    }

    #[test]
    fn tool_stats_track_calls_errors_and_latency() {
        let mut stats = SessionStats::default();
        stats.fold(&tool_finished("payment", ToolStatus::Ok, 30));
        stats.fold(&tool_finished("payment", ToolStatus::Timeout, 5_000));
        stats.fold(&tool_finished("search", ToolStatus::Substituted, 0));

        assert_eq!(stats.total_tool_calls, 3);
        let payment = &stats.tools["payment"];
        assert_eq!(payment.calls, 2);
        assert_eq!(payment.errors, 1);
        assert_eq!(payment.total_duration_ms, 5_030);
        assert_eq!(payment.max_duration_ms, 5_000);
        assert_eq!(
            stats.tools["search"].errors, 0,
            "substitution is not execution failure"
        );
    }

    #[test]
    fn tracked_maps_are_bounded_with_an_overflow_bucket() {
        let mut stats = SessionStats::default();
        for i in 0..(MAX_TRACKED_TOOLS + 5) {
            stats.fold(&tool_finished(&format!("tool-{i}"), ToolStatus::Ok, 1));
        }
        assert_eq!(stats.tools.len(), MAX_TRACKED_TOOLS);
        assert_eq!(stats.other_tools.calls, 5);
        assert_eq!(stats.total_tool_calls as usize, MAX_TRACKED_TOOLS + 5);
    }

    #[test]
    fn delegation_edges_fold_into_delegated_usage() {
        let mut stats = SessionStats::default();
        stats.fold(&AgentEvent::DelegationFinished {
            run_id: "r1".into(),
            turn: 1,
            call_id: "c".into(),
            agent: "scout".into(),
            status: DelegationStatus::Ok,
            usage: TokenUsage {
                input_tokens: 9,
                output_tokens: 4,
                ..Default::default()
            },
            model: None,
            duration_ms: 40,
            child_session: None,
            child_persistence: None,
            at: Utc::now(),
        });
        stats.fold(&AgentEvent::DelegationFinished {
            run_id: "r1".into(),
            turn: 1,
            call_id: "c2".into(),
            agent: "scout".into(),
            status: DelegationStatus::Failed("x".into()),
            usage: TokenUsage::default(),
            model: None,
            duration_ms: 1,
            child_session: None,
            child_persistence: None,
            at: Utc::now(),
        });

        assert_eq!(stats.delegations, 2);
        assert_eq!(stats.delegation_errors, 1);
        assert_eq!(stats.delegated_usage.input_tokens, 9);
        assert_eq!(stats.input_tokens, 0, "own tokens stay separate");
    }

    #[test]
    fn a_snapshot_with_stats_is_authoritative() {
        let mut stats = SessionStats::default();
        stats.fold(&tool_result("payment"));

        let rolled = SessionStats {
            runs: 9,
            total_tool_calls: 42,
            ..Default::default()
        };
        stats.fold(&AgentEvent::StateSnapshot {
            run_id: "r1".into(),
            messages: vec![],
            system_prompt: "sys".into(),
            reason: "compaction".into(),
            stats: Some(Box::new(rolled.clone())),
            open_tasks: None,
            data: None,
            at: Utc::now(),
        });
        assert_eq!(stats, rolled);
    }

    #[test]
    fn a_stats_less_snapshot_keeps_the_folded_stats() {
        let mut stats = SessionStats::default();
        stats.fold(&tool_result("payment"));

        stats.fold(&AgentEvent::StateSnapshot {
            run_id: "r1".into(),
            messages: vec![],
            system_prompt: "sys".into(),
            reason: "old row".into(),
            stats: None,
            open_tasks: None,
            data: None,
            at: Utc::now(),
        });
        assert_eq!(stats.total_tool_calls, 1);
    }
}
