use std::sync::Arc;

use async_trait::async_trait;
use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_subagent::{AgentDef, ChildBuilder, DelegationCtx, subagents};
use runic_types::{ContentBlock, StopReason, TokenUsage};

fn write_agent_file(root: &std::path::Path, name: &str, body: &str) {
    std::fs::write(root.join(name), body).unwrap();
}

fn write_agent_dir(root: &std::path::Path, dir: &str, body: &str) {
    let dir = root.join(dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("AGENT.md"), body).unwrap();
}

struct OneShot;

#[async_trait]
impl Provider for OneShot {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "done".into(),
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage::default(),
        })
    }
}

struct FakeChildBuilder;

#[async_trait]
impl ChildBuilder for FakeChildBuilder {
    async fn build(&self, def: &AgentDef, _dctx: &DelegationCtx) -> anyhow::Result<Agent> {
        Ok(Agent::builder(Arc::new(OneShot), "u", &def.name)
            .model("test")
            .system_prompt(def.system_prompt.clone())
            .build())
    }
}

#[test]
fn builder_loads_top_level_and_nested_agents_and_skips_invalid_entries() {
    let root = tempfile::tempdir().unwrap();
    write_agent_file(
        root.path(),
        "reviewer.md",
        "---\nname: reviewer\ndescription: reviews\n---\nReview.",
    );
    write_agent_dir(
        root.path(),
        "researcher",
        "---\nname: researcher\ndescription: researches\n---\nResearch.",
    );
    write_agent_dir(
        root.path(),
        "invalid",
        "---\nname: \ndescription: invalid\n---\nInvalid.",
    );
    std::fs::write(root.path().join("ignored.txt"), "ignored").unwrap();

    let roster = subagents(root.path()).roster();

    assert_eq!(roster.len(), 2);
    assert!(roster.get("reviewer").is_some());
    assert!(roster.get("researcher").is_some());
    assert!(roster.get("invalid").is_none());
}

#[test]
fn builder_accepts_multiple_dirs_and_skips_missing_dirs() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_agent_file(
        first.path(),
        "alpha.md",
        "---\nname: alpha\ndescription: first\n---\nAlpha.",
    );
    write_agent_file(
        second.path(),
        "beta.md",
        "---\nname: beta\ndescription: second\n---\nBeta.",
    );

    let roster = subagents(vec![
        first.path().to_path_buf(),
        second.path().to_path_buf(),
        second.path().join("missing"),
    ])
    .roster();

    assert_eq!(roster.len(), 2);
    assert!(roster.get("alpha").is_some());
    assert!(roster.get("beta").is_some());
}

#[test]
fn delegate_tool_is_exposed_only_for_non_empty_rosters() {
    let root = tempfile::tempdir().unwrap();
    write_agent_file(
        root.path(),
        "reviewer.md",
        "---\nname: reviewer\ndescription: reviews\n---\nReview.",
    );

    assert!(
        subagents(root.path())
            .tool(Arc::new(FakeChildBuilder))
            .is_some()
    );

    let empty = tempfile::tempdir().unwrap();
    assert!(
        subagents(empty.path())
            .tool(Arc::new(FakeChildBuilder))
            .is_none()
    );
}
