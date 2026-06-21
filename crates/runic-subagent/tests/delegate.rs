//! End-to-end delegate tests: a fake `ChildBuilder` builds scripted child
//! agents, exercising the four safeguards + the action surface.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_subagent::{
    AgentDef, AgentRoster, ChildBuilder, DelegateTool, DelegationCtx, SpawnBudget,
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
impl ChildBuilder for FakeBuilder {
    async fn build(&self, def: &AgentDef, _dctx: &DelegationCtx) -> anyhow::Result<Agent> {
        let provider = Arc::new(OneShot(format!("done: {}", def.name)));
        Ok(Agent::builder(provider, "u", "s")
            .model("test")
            .system_prompt(def.system_prompt.clone())
            .build())
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
    assert!(r.success);
    assert_eq!(r.output, "done: reviewer");
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
    assert!(!r.success);
    assert!(r.output.contains("reviewer")); // roster surfaced
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
    assert!(!r.success);
    assert!(r.output.contains("depth limit"));
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
    assert!(first.success);
    let second = tool
        .execute(
            serde_json::json!({ "agent": "researcher", "prompt": "y" }),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(!second.success);
    assert!(second.output.contains("budget"));
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
    assert!(r.success);
    assert!(r.output.contains("done: reviewer"));
    assert!(r.output.contains("done: researcher"));
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
    assert!(start.success);
    // Extract the task id from "...task_id=task-xxxx".
    let task_id = start
        .output
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
        if r.output == "done: reviewer" {
            output = Some(r.output);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(output.as_deref(), Some("done: reviewer"));
}
