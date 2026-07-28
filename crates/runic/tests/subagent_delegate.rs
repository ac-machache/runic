use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use runic::Llm;
use runic::ability::ability;
use runic::composer::Agent;
use runic::subagent::{DelegateTool, Invocation, SpawnBudget, Subagent};
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

fn child(reply: &str) -> Agent {
    Agent::new(Llm::new(Arc::new(OneShot(reply.into())), "test"))
}

fn roster() -> Vec<Subagent> {
    vec![
        Subagent::new("reviewer", "reviews", child("done: reviewer")),
        Subagent::new("researcher", "researches", child("done: researcher")),
    ]
}

fn ctx() -> ToolContext {
    ToolContext::new("u", "s", "r")
}

#[tokio::test]
async fn delegate_sync_returns_child_answer() {
    let tool = DelegateTool::new(roster());
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

    fn points(&self) -> &'static [runic_hook::HookLifecycle] {
        &[runic_hook::HookLifecycle::BeforeModel]
    }

    async fn before_model(&self, _state: &mut runic_state::AgentState) -> runic_hook::HookOutcome {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        runic_hook::HookOutcome::Noop
    }
}

#[tokio::test]
async fn subagent_hook_fires_on_the_child() {
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let roster = vec![Subagent::new(
        "reviewer",
        "reviews",
        Agent::new(Llm::new(Arc::new(OneShot("ok".into())), "m").instructions("Review."))
            .with(ability("flag").hook(FlagHook(fired.clone()))),
    )];
    let tool = DelegateTool::new(roster);
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
    let roster = vec![Subagent::new(
        "specialist",
        "own brain",
        Agent::new(
            Llm::new(Arc::new(OneShot("from own llm".into())), "own-model").instructions("hi"),
        ),
    )];
    let tool = DelegateTool::new(roster);
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
async fn voice_knobs_render_section_and_rename_the_tool() {
    let tool = DelegateTool::new(roster())
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
    let tool = DelegateTool::new(roster()).tool_name("dispatch");
    let section = tool.roster_section();
    assert!(section.starts_with("<subagents>"));
    assert!(section.contains("via the `dispatch` tool"));
    assert!(!section.contains("`delegate`"));
}

#[tokio::test]
async fn unknown_agent_lists_roster() {
    let tool = DelegateTool::new(roster());
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
    let tool = DelegateTool::new(roster()).with_depth(3).with_max_depth(3);
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
    let tool = DelegateTool::new(roster()).with_budget(SpawnBudget::new(1, 4)); // total lifetime cap = 1
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
    let tool = DelegateTool::new(roster());
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
    let tool = DelegateTool::new(roster());
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

#[tokio::test]
async fn a_background_only_subagent_detaches_even_when_the_model_asks_to_block() {
    let roster = vec![
        Subagent::new("slow", "takes ages", child("eventually")).invocation(Invocation::Background),
    ];
    let tool = DelegateTool::new(roster);

    let r = tool
        .execute(
            serde_json::json!({ "agent": "slow", "prompt": "go", "background": false }),
            &ctx(),
        )
        .await
        .unwrap();

    assert!(!r.is_error());
    assert!(
        r.text().contains("task_id="),
        "it ran detached despite background:false — {}",
        r.text()
    );
    assert!(
        r.text().contains("invocation=background"),
        "and the model is told why, in the same result: {}",
        r.text()
    );
}

#[tokio::test]
async fn a_sync_only_subagent_blocks_even_when_the_model_asks_to_detach() {
    let roster =
        vec![Subagent::new("quick", "fast", child("done now")).invocation(Invocation::Sync)];
    let tool = DelegateTool::new(roster);

    let r = tool
        .execute(
            serde_json::json!({ "agent": "quick", "prompt": "go", "background": true }),
            &ctx(),
        )
        .await
        .unwrap();

    assert!(!r.is_error());
    assert!(r.text().contains("done now"), "{}", r.text());
    assert!(!r.text().contains("task_id="), "{}", r.text());
}

#[tokio::test]
async fn invocation_any_leaves_the_choice_to_the_model_and_adds_no_note() {
    let tool = DelegateTool::new(roster());
    let r = tool
        .execute(
            serde_json::json!({ "agent": "reviewer", "prompt": "go", "background": false }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(r.text(), "done: reviewer");
}

#[test]
fn the_roster_line_advertises_a_constrained_invocation() {
    let sub = Subagent::new("slow", "takes ages", child("x")).invocation(Invocation::Background);
    assert!(sub.roster_line().contains("runs in the background"));
    assert!(
        Subagent::new("plain", "normal", child("x"))
            .roster_line()
            .ends_with("normal")
    );
}

#[tokio::test]
async fn a_parallel_batch_is_rejected_when_a_member_is_background_only() {
    let roster = vec![
        Subagent::new("reviewer", "reviews", child("done: reviewer")),
        Subagent::new("slow", "takes ages", child("eventually")).invocation(Invocation::Background),
    ];
    let tool = DelegateTool::new(roster);

    let r = tool
        .execute(
            serde_json::json!({ "parallel": ["reviewer", "slow"], "prompt": "go" }),
            &ctx(),
        )
        .await
        .unwrap();

    assert!(r.is_error(), "{}", r.text());
    assert!(r.text().contains("`slow`"), "{}", r.text());
    assert!(
        r.text().contains("background=true"),
        "the model is told how to fix it: {}",
        r.text()
    );
}

#[tokio::test]
async fn a_parallel_batch_of_unconstrained_subagents_still_runs() {
    let tool = DelegateTool::new(roster());
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
