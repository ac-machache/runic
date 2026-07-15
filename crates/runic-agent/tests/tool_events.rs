mod harness;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use harness::*;
use runic_agent::Agent;
use runic_state::{SessionEvent, ToolStatus};
use runic_tool::{Tool, ToolContext, ToolResult};

struct SleepTool {
    name: &'static str,
    ms: u64,
}

#[async_trait]
impl Tool for SleepTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "sleeps"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        tokio::time::sleep(Duration::from_millis(self.ms)).await;
        Ok(ToolResult::ok("done"))
    }
}

fn finished(evs: &[SessionEvent]) -> Vec<(String, ToolStatus, u64, u32)> {
    evs.iter()
        .filter_map(|e| match e {
            SessionEvent::ToolFinished {
                tool,
                status,
                duration_ms,
                turn,
                ..
            } => Some((tool.clone(), *status, *duration_ms, *turn)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn serial_tool_durations_exclude_earlier_batch_mates() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        multi_tool_response(vec![
            ("c1", "slow", serde_json::json!({})),
            ("c2", "fast", serde_json::json!({})),
        ]),
        text_response("done"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(SleepTool {
            name: "slow",
            ms: 80,
        }))
        .tool(Arc::new(SleepTool {
            name: "fast",
            ms: 1,
        }))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap();

    let done = finished(&drain_session(&mut events));
    assert_eq!(done.len(), 2);
    let slow = done.iter().find(|(t, ..)| t == "slow").unwrap();
    let fast = done.iter().find(|(t, ..)| t == "fast").unwrap();
    assert_eq!(slow.1, ToolStatus::Ok);
    assert_eq!(fast.1, ToolStatus::Ok);
    assert!(slow.2 >= 60, "slow ran for its own time: {}ms", slow.2);
    assert!(
        fast.2 < 60,
        "fast must not absorb slow's runtime: {}ms",
        fast.2
    );
    assert_eq!(slow.3, 1, "tool events carry the turn");

    let starts = drain_session(&mut events);
    assert!(starts.is_empty());
}

#[tokio::test]
async fn unknown_tools_get_a_distinct_durable_status() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("c1", "ghost", serde_json::json!({})),
        text_response("done"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap();

    let evs = drain_session(&mut events);
    let done = finished(&evs);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].1, ToolStatus::UnknownTool);
    assert!(
        evs.iter()
            .any(|e| matches!(e, SessionEvent::ToolStarted { tool, .. } if tool == "ghost")),
        "an attempted dispatch still records its start"
    );
}
