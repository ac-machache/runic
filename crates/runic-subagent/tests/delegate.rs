//! End-to-end delegate tests: a fake `SubagentBuilder` builds scripted child
//! agents, exercising the four safeguards + the action surface.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_subagent::{
    AgentDef, AgentRoster, DelegateTool, SpawnBudget, SubagentBuilder, SubagentReq,
};
use runic_tool::{Tool, ToolContext};
use runic_types::{ContentBlock, StopReason, TokenUsage};

/// A provider that returns one canned text response.
struct OneShot(String);

#[async_trait]
impl Provider for OneShot {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: self.0.clone(),
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage::default(),
        })
    }
}

/// Builds a child whose only reply is `done: <agent name>`.
struct FakeBuilder;

#[async_trait]
impl SubagentBuilder for FakeBuilder {
    async fn provider(&self, req: &SubagentReq<'_>) -> Arc<dyn Provider> {
        Arc::new(OneShot(format!("done: {}", req.def.name)))
    }

    fn default_model(&self, _req: &SubagentReq<'_>) -> String {
        "test".to_string()
    }
}

fn roster() -> Arc<AgentRoster> {
    Arc::new(AgentRoster::new(vec![
        AgentDef::parse_markdown("---\nname: reviewer\ndescription: reviews\n---\nReview things.")
            .unwrap(),
        AgentDef::parse_markdown("---\nname: researcher\ndescription: researches\n---\nResearch.")
            .unwrap(),
    ]))
}

fn ctx() -> ToolContext {
    ToolContext::new("u", "s", "r")
}

#[tokio::test]
async fn delegate_sync_returns_child_answer() {
    let tool = DelegateTool::new(roster(), Arc::new(FakeBuilder));
    let r = tool
        .execute(
            serde_json::json!({ "agent": "reviewer", "prompt": "look at this" }),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(!r.is_error());
    assert_eq!(r.text(), "done: reviewer");
}

#[tokio::test]
async fn unknown_agent_lists_roster() {
    let tool = DelegateTool::new(roster(), Arc::new(FakeBuilder));
    let r = tool
        .execute(
            serde_json::json!({ "agent": "ghost", "prompt": "x" }),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(r.is_error());
    assert!(r.text().contains("reviewer")); // roster surfaced
}

#[tokio::test]
async fn depth_limit_refuses_delegation() {
    let tool = DelegateTool::new(roster(), Arc::new(FakeBuilder))
        .with_depth(3)
        .with_max_depth(3);
    let r = tool
        .execute(
            serde_json::json!({ "agent": "reviewer", "prompt": "x" }),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(r.is_error());
    assert!(r.text().contains("depth limit"));
}

#[tokio::test]
async fn spawn_budget_caps_total() {
    let tool =
        DelegateTool::new(roster(), Arc::new(FakeBuilder)).with_budget(SpawnBudget::new(1, 4)); // total lifetime cap = 1
    let first = tool
        .execute(
            serde_json::json!({ "agent": "reviewer", "prompt": "x" }),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(!first.is_error());
    let second = tool
        .execute(
            serde_json::json!({ "agent": "researcher", "prompt": "y" }),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(second.is_error());
    assert!(second.text().contains("budget"));
}

#[tokio::test]
async fn parallel_runs_several_and_aggregates() {
    let tool = DelegateTool::new(roster(), Arc::new(FakeBuilder));
    let r = tool
        .execute(
            serde_json::json!({ "parallel": ["reviewer", "researcher"], "prompt": "go" }),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(!r.is_error());
    assert!(r.text().contains("done: reviewer"));
    assert!(r.text().contains("done: researcher"));
}

#[tokio::test]
async fn background_then_check_result() {
    let tool = DelegateTool::new(roster(), Arc::new(FakeBuilder));
    let start = tool
        .execute(
            serde_json::json!({ "agent": "reviewer", "prompt": "x", "background": true }),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(!start.is_error());
    // Extract the task id from "...task_id=task-xxxx".
    let task_id = start
        .text()
        .split("task_id=")
        .nth(1)
        .unwrap()
        .trim()
        .to_string();

    // Poll until the detached task finishes.
    let mut output = None;
    for _ in 0..50 {
        let r = tool
            .execute(
                serde_json::json!({ "action": "check_result", "task_id": task_id }),
                &ctx(),
            )
            .await
            .unwrap();
        if r.text() == "done: reviewer" {
            output = Some(r.text());
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(output.as_deref(), Some("done: reviewer"));
}
