use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic_foundry::{Assembly, assemble};
use runic_memory::{Target, memory};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_skills::SkillSet;
use runic_subagent::subagents;
use runic_substrate::sessions_memory;
use runic_tools::tools;
use runic_types::{ContentBlock, StopReason, TokenUsage};

#[derive(Default)]
struct RecordingProvider {
    requests: Mutex<Vec<CompletionRequest>>,
}

impl RecordingProvider {
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn last_request(&self) -> CompletionRequest {
        self.requests.lock().unwrap().last().unwrap().clone()
    }
}

#[async_trait]
impl Provider for RecordingProvider {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        Ok(text_response("ok"))
    }
}

fn text_response(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: text.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage::default(),
    }
}

fn base_assembly(provider: Arc<dyn Provider>) -> Assembly {
    Assembly {
        provider,
        model: "model-a".into(),
        instructions: "core instructions".into(),
        memory: None,
        skills: None,
        subagents: None,
        mcp: None,
        sessions: None,
        tools: None,
    }
}

fn write_skill(root: &std::path::Path, dir: &str, name: &str, description: &str) {
    let dir = root.join(dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nUse the workflow."),
    )
    .unwrap();
}

fn write_agent(root: &std::path::Path, dir: &str, name: &str, description: &str) {
    let dir = root.join(dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("AGENT.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nAct carefully."),
    )
    .unwrap();
}

#[tokio::test]
async fn assemble_composes_prompt_sections_in_order() {
    let provider = Arc::new(RecordingProvider::default());
    let memory_dir = tempfile::tempdir().unwrap();
    let skill_dir = tempfile::tempdir().unwrap();
    let agent_dir = tempfile::tempdir().unwrap();

    let memory_cfg = memory(memory_dir.path()).init().scope_per_tenant();
    let store = memory_cfg.store("alice");
    store
        .add(Target::Memory, "project uses focused tests")
        .await
        .unwrap();
    store
        .add(Target::User, "user prefers direct prose")
        .await
        .unwrap();

    write_skill(skill_dir.path(), "review", "review", "reviews code");
    write_agent(agent_dir.path(), "researcher", "researcher", "researches");

    let mut assembly = base_assembly(provider);
    assembly.memory = Some(memory_cfg);
    assembly.skills = Some(Arc::new(SkillSet::load_dir("", skill_dir.path()).await));
    assembly.subagents = Some(subagents(agent_dir.path()));

    let agent = assemble(&assembly, "alice", "s1").await;
    let system = &agent.state().system_prompt;

    let instructions = system.find("core instructions").unwrap();
    let memory = system.find("project uses focused tests").unwrap();
    let user = system.find("user prefers direct prose").unwrap();
    let skills = system.find("<available-skills>").unwrap();
    let subagents = system.find("<subagents>").unwrap();

    assert!(instructions < memory);
    assert!(memory < user);
    assert!(user < skills);
    assert!(skills < subagents);
}

#[tokio::test]
async fn assemble_registers_enabled_tool_surfaces() {
    let provider = Arc::new(RecordingProvider::default());
    let skill_dir = tempfile::tempdir().unwrap();
    let agent_dir = tempfile::tempdir().unwrap();
    let memory_dir = tempfile::tempdir().unwrap();

    write_skill(skill_dir.path(), "review", "review", "reviews code");
    write_agent(agent_dir.path(), "researcher", "researcher", "researches");

    let mut assembly = base_assembly(provider.clone());
    assembly.tools = Some(tools().web().weather().hitl());
    assembly.memory = Some(memory(memory_dir.path()).init().include_mem_tools());
    assembly.skills = Some(Arc::new(SkillSet::load_dir("", skill_dir.path()).await));
    assembly.subagents = Some(subagents(agent_dir.path()));
    assembly.sessions = Some(sessions_memory());

    let mut agent = assemble(&assembly, "alice", "s1").await;
    agent.run("hello").await.unwrap();

    let mut names: Vec<String> = provider
        .last_request()
        .tools
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    names.sort();

    for expected in [
        "calculator",
        "system_time",
        "web_fetch",
        "weather",
        "weather_history",
        "ask_user",
        "escalate_to_human",
        "memory",
        "skill_view",
        "delegate",
        "search_chats",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing {expected}"
        );
    }
    // file tools were removed with runic-filesystem — never registered
    for absent in [
        "read_file",
        "write_file",
        "edit_file",
        "ls",
        "glob",
        "grep",
        "apply_patch",
    ] {
        assert!(
            !names.iter().any(|name| name == absent),
            "unexpected {absent}"
        );
    }
}

#[tokio::test]
async fn assemble_omits_optional_prompt_sections_and_tools_when_empty() {
    let provider = Arc::new(RecordingProvider::default());
    let mut agent = assemble(&base_assembly(provider.clone()), "alice", "s1").await;

    assert_eq!(agent.state().system_prompt, "core instructions");
    agent.run("hello").await.unwrap();

    assert!(provider.last_request().tools.is_empty());
}

#[tokio::test]
async fn memory_review_is_disabled_by_default() {
    let provider = Arc::new(RecordingProvider::default());
    let memory_dir = tempfile::tempdir().unwrap();
    let mut assembly = base_assembly(provider.clone());
    assembly.memory = Some(memory(memory_dir.path()).init());

    let mut agent = assemble(&assembly, "alice", "s1").await;
    agent.run("hello").await.unwrap();

    assert_eq!(provider.count(), 1);
}

#[tokio::test]
async fn memory_review_spawns_when_interval_is_due() {
    let provider = Arc::new(RecordingProvider::default());
    let memory_dir = tempfile::tempdir().unwrap();
    let mut assembly = base_assembly(provider.clone());
    assembly.memory = Some(
        memory(memory_dir.path())
            .init()
            .include_mem_tools()
            .review(1),
    );

    let mut agent = assemble(&assembly, "alice", "s1").await;
    agent.run("hello").await.unwrap();

    for _ in 0..50 {
        if provider.count() >= 2 {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(provider.count(), 2);
    let review_request = provider.last_request();
    assert_eq!(review_request.model, "model-a");
    assert!(
        review_request
            .system
            .as_deref()
            .is_some_and(|system| system.contains("Review the conversation"))
    );
    assert!(
        review_request
            .tools
            .iter()
            .any(|tool| tool.name == "memory")
    );
}

#[tokio::test]
async fn memory_review_waits_until_interval() {
    let provider = Arc::new(RecordingProvider::default());
    let memory_dir = tempfile::tempdir().unwrap();
    let mut assembly = base_assembly(provider.clone());
    assembly.memory = Some(
        memory(memory_dir.path())
            .init()
            .include_mem_tools()
            .review(2),
    );

    let mut agent = assemble(&assembly, "alice", "s1").await;
    agent.run("hello").await.unwrap();

    assert_eq!(provider.count(), 1);
}
