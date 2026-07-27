//! End-to-end delegate tests: a fake `SubagentBuilder` builds scripted child
//! agents, exercising the four safeguards + the action surface.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use runic::subagent::{DelegateTool, SpawnBudget, Subagent, SubagentBuilder, SubagentReq};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
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
        Arc::new(OneShot(format!("done: {}", req.subagent.name)))
    }

    fn default_model(&self, _req: &SubagentReq<'_>) -> String {
        "test".to_string()
    }
}

fn roster() -> Vec<Subagent> {
    vec![
        Subagent::new("reviewer", "reviews").prompt("Review things."),
        Subagent::new("researcher", "researches").prompt("Research."),
    ]
}

fn ctx() -> ToolContext {
    ToolContext::new("u", "s", "r")
}

#[tokio::test]
async fn delegate_sync_returns_child_answer() {
    let tool = DelegateTool::with_builder(roster(), Arc::new(FakeBuilder));
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

struct FlagHook(Arc<std::sync::atomic::AtomicBool>);

#[async_trait]
impl runic_hook::WriteHook for FlagHook {
    fn name(&self) -> &str {
        "flag"
    }

    async fn before_agent(&self, _state: &mut runic_state::AgentState) -> runic_hook::HookOutcome {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        runic_hook::HookOutcome::Noop
    }
}

#[tokio::test]
async fn subagent_hook_fires_on_the_child() {
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let roster = vec![
        Subagent::new("reviewer", "reviews")
            .prompt("Review.")
            .hook(FlagHook(fired.clone())),
    ];
    let provider: Arc<dyn Provider> = Arc::new(OneShot("ok".into()));
    let tool = DelegateTool::new(roster, provider, "m");
    tool.execute(
        serde_json::json!({ "agent": "reviewer", "prompt": "go" }),
        &ctx(),
    )
    .await
    .unwrap();
    assert!(
        fired.load(std::sync::atomic::Ordering::SeqCst),
        "the subagent's hook must run in the child loop"
    );
}

#[tokio::test]
async fn subagent_with_own_llm_uses_its_provider() {
    let roster =
        vec![
            Subagent::new("specialist", "own brain")
                .prompt("hi")
                .llm(runic_agent::Llm::new(
                    Arc::new(OneShot("from own llm".into())),
                    "own-model",
                )),
        ];
    let parent: Arc<dyn Provider> = Arc::new(OneShot("from parent".into()));
    let tool = DelegateTool::new(roster, parent, "parent-model");
    let r = tool
        .execute(
            serde_json::json!({ "agent": "specialist", "prompt": "go" }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(r.text(), "from own llm");
}

#[tokio::test]
async fn new_runs_children_without_a_builder_impl() {
    let provider: Arc<dyn Provider> = Arc::new(OneShot("child says hi".into()));
    let tool = DelegateTool::new(roster(), provider, "mistral-large-latest");
    let r = tool
        .execute(
            serde_json::json!({ "agent": "reviewer", "prompt": "go" }),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(!r.is_error());
    assert_eq!(r.text(), "child says hi");
}

#[tokio::test]
async fn voice_knobs_render_section_and_rename_the_tool() {
    let provider: Arc<dyn Provider> = Arc::new(OneShot("x".into()));
    let tool = DelegateTool::new(roster(), provider, "m")
        .tag("team")
        .intro("Hand self-contained work to your team:")
        .tool_name("dispatch")
        .tool_description("Send a teammate a task.");

    let section = tool.roster_section();
    assert!(section.starts_with("<team>\n"));
    assert!(section.ends_with("</team>"));
    assert!(section.contains("Hand self-contained work to your team:"));
    assert!(section.contains("- reviewer: reviews"));
    assert!(!section.contains("subagents"));

    assert_eq!(tool.name(), "dispatch");
    assert_eq!(tool.description(), "Send a teammate a task.");
}

#[tokio::test]
async fn default_intro_interpolates_a_renamed_tool() {
    let provider: Arc<dyn Provider> = Arc::new(OneShot("x".into()));
    let tool = DelegateTool::new(roster(), provider, "m").tool_name("dispatch");
    let section = tool.roster_section();
    assert!(section.starts_with("<subagents>"));
    assert!(section.contains("via the `dispatch` tool"));
    assert!(!section.contains("`delegate`"));
}

#[tokio::test]
async fn unknown_agent_lists_roster() {
    let tool = DelegateTool::with_builder(roster(), Arc::new(FakeBuilder));
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
    let tool = DelegateTool::with_builder(roster(), Arc::new(FakeBuilder))
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
    let tool = DelegateTool::with_builder(roster(), Arc::new(FakeBuilder))
        .with_budget(SpawnBudget::new(1, 4)); // total lifetime cap = 1
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
    let tool = DelegateTool::with_builder(roster(), Arc::new(FakeBuilder));
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
    let tool = DelegateTool::with_builder(roster(), Arc::new(FakeBuilder));
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
