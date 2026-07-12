use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::{
    Compaction as CompactionAbility, Delegation, Memory, Sessions, Skills, Tools, Toolset,
};
use runic::composer::Composer;
use runic_memory::{Target, memory};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_skills::SkillSet;
use runic_subagent::subagents;
use runic_substrate::sessions_memory;
use runic_tool::{Tool, ToolContext, ToolResult};
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

    fn first_request(&self) -> CompletionRequest {
        self.requests.lock().unwrap().first().unwrap().clone()
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

fn base(provider: Arc<dyn Provider>) -> Composer {
    Composer::new(provider, "model-a").instructions("core instructions")
}

async fn write_skill(root: &std::path::Path, dir: &str, name: &str, description: &str) {
    let dir = root.join(dir);
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nUse the workflow."),
    )
    .await
    .unwrap();
}

async fn write_agent(root: &std::path::Path, dir: &str, name: &str, description: &str) {
    let dir = root.join(dir);
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(
        dir.join("AGENT.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nAct carefully."),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn composes_prompt_sections_in_order() {
    let provider = Arc::new(RecordingProvider::default());
    let memory_dir = tempfile::tempdir().unwrap();
    let skill_dir = tempfile::tempdir().unwrap();
    let agent_dir = tempfile::tempdir().unwrap();

    let memory_cfg = memory(memory_dir.path()).init().scope_per_tenant();
    let store = memory_cfg.store("alice").await;
    store
        .add(Target::Memory, "project uses focused tests")
        .await
        .unwrap();
    store
        .add(Target::User, "user prefers direct prose")
        .await
        .unwrap();

    write_skill(skill_dir.path(), "review", "review", "reviews code").await;
    write_agent(agent_dir.path(), "researcher", "researcher", "researches").await;

    let agent = base(provider)
        .with(Memory(memory_cfg))
        .with(Skills(Arc::new(
            SkillSet::load_dir("", skill_dir.path()).await,
        )))
        .with(Delegation(subagents(agent_dir.path())))
        .build("alice", "s1")
        .await
        .unwrap();
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
async fn registers_enabled_tool_surfaces() {
    let provider = Arc::new(RecordingProvider::default());
    let skill_dir = tempfile::tempdir().unwrap();
    let agent_dir = tempfile::tempdir().unwrap();
    let memory_dir = tempfile::tempdir().unwrap();

    write_skill(skill_dir.path(), "review", "review", "reviews code").await;
    write_agent(agent_dir.path(), "researcher", "researcher", "researches").await;

    let mut agent = base(provider.clone())
        .with(Toolset(tools().web().weather().hitl()))
        .with(Memory(
            memory(memory_dir.path()).init().include_memory_tool(),
        ))
        .with(Skills(Arc::new(
            SkillSet::load_dir("", skill_dir.path()).await,
        )))
        .with(Delegation(subagents(agent_dir.path())))
        .with(Sessions(sessions_memory()))
        .build("alice", "s1")
        .await
        .unwrap();
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
async fn memory_tool_description_override_reaches_the_model() {
    let provider = Arc::new(RecordingProvider::default());
    let memory_dir = tempfile::tempdir().unwrap();

    let mut agent = base(provider.clone())
        .with(Memory(
            memory(memory_dir.path())
                .init()
                .include_memory_tool()
                .memory_tool_description("notes for a support bot"),
        ))
        .build("alice", "s1")
        .await
        .unwrap();
    agent.run("hello").await.unwrap();

    let tool = provider
        .last_request()
        .tools
        .into_iter()
        .find(|t| t.name == "memory")
        .expect("memory tool registered");
    assert_eq!(tool.description, "notes for a support bot");
}

#[tokio::test]
async fn omits_optional_prompt_sections_and_tools_when_empty() {
    let provider = Arc::new(RecordingProvider::default());
    let mut agent = base(provider.clone()).build("alice", "s1").await.unwrap();

    assert_eq!(agent.state().system_prompt, "core instructions");
    agent.run("hello").await.unwrap();

    assert!(provider.last_request().tools.is_empty());
}

#[tokio::test]
async fn compaction_folds_history_before_the_model_call() {
    let provider = Arc::new(RecordingProvider::default());
    let mut agent = base(provider.clone())
        .with(CompactionAbility(
            runic::Compaction::new()
                .max_context_tokens(12)
                .keep_recent(2),
        ))
        .build("alice", "s1")
        .await
        .unwrap();
    let old = [
        runic_types::Message::user("x".repeat(40)),
        runic_types::Message::assistant("y".repeat(40)),
        runic_types::Message::user("recent question"),
    ];
    for msg in old {
        agent
            .state_mut()
            .push_event(runic_state::SessionEvent::Message {
                run_id: "r0".into(),
                msg,
                at: chrono::Utc::now(),
            });
    }

    agent.run("final question").await.unwrap();

    assert_eq!(
        provider.count(),
        2,
        "summarizer call + the turn's model call"
    );
    let turn_request = provider.last_request();
    assert_eq!(turn_request.messages.len(), 3);
    assert_eq!(turn_request.messages[0].role, runic_types::Role::Assistant);
    assert!(
        turn_request.messages[0]
            .content
            .text_content()
            .contains("Conversation summary")
    );
    assert!(
        turn_request.messages[1]
            .content
            .text_content()
            .contains("recent question")
    );
    assert!(
        agent
            .state()
            .events()
            .iter()
            .any(|e| matches!(e, runic_state::SessionEvent::StateSnapshot { .. }))
    );
}

#[tokio::test]
async fn summary_guidance_override_reaches_the_summarizer() {
    let provider = Arc::new(RecordingProvider::default());
    let mut agent = base(provider.clone())
        .with(CompactionAbility(
            runic::Compaction::new()
                .max_context_tokens(12)
                .keep_recent(2)
                .summary_guidance("custom summary instructions"),
        ))
        .build("alice", "s1")
        .await
        .unwrap();
    for msg in [
        runic_types::Message::user("x".repeat(40)),
        runic_types::Message::assistant("y".repeat(40)),
        runic_types::Message::user("recent question"),
    ] {
        agent
            .state_mut()
            .push_event(runic_state::SessionEvent::Message {
                run_id: "r0".into(),
                msg,
                at: chrono::Utc::now(),
            });
    }

    agent.run("final question").await.unwrap();

    assert_eq!(provider.count(), 2);
    assert_eq!(
        provider.first_request().system.as_deref(),
        Some("custom summary instructions")
    );
}

#[tokio::test]
async fn compaction_sweeps_notified_keys_of_departed_tasks() {
    let provider = Arc::new(RecordingProvider::default());
    let mut agent = base(provider.clone())
        .with(CompactionAbility(
            runic::Compaction::new()
                .max_context_tokens(12)
                .keep_recent(2),
        ))
        .build("alice", "s1")
        .await
        .unwrap();

    let state = agent.state_mut();
    state.fold_event(runic_state::SessionEvent::TaskSpawned {
        run_id: "r0".into(),
        task_id: "t-done".into(),
        agent: "scout".into(),
        prompt: "dig".into(),
        at: chrono::Utc::now(),
    });
    state.fold_event(runic_state::SessionEvent::TaskFinished {
        run_id: "r0".into(),
        task_id: "t-done".into(),
        status: runic_state::TaskStatus::Completed,
        result: Some("gold".into()),
        at: chrono::Utc::now(),
    });
    state
        .update("task-reminder/notified/t-done", serde_json::json!(true))
        .unwrap();
    state.update("keep/me", serde_json::json!(1)).unwrap();
    for msg in [
        runic_types::Message::user("x".repeat(40)),
        runic_types::Message::assistant("y".repeat(40)),
        runic_types::Message::user("recent question"),
    ] {
        state.push_event(runic_state::SessionEvent::Message {
            run_id: "r0".into(),
            msg,
            at: chrono::Utc::now(),
        });
    }

    agent.run("final question").await.unwrap();

    assert!(
        agent
            .state()
            .events()
            .iter()
            .any(|e| matches!(e, runic_state::SessionEvent::StateSnapshot { .. }))
    );
    assert!(
        agent.state().get("task-reminder/notified/t-done").is_none(),
        "notified key of a compacted-away task must not survive the snapshot"
    );
    assert_eq!(agent.state().get("keep/me"), Some(&serde_json::json!(1)));
    assert!(agent.state().tasks().is_empty());
}

#[tokio::test]
async fn memory_review_is_disabled_by_default() {
    let provider = Arc::new(RecordingProvider::default());
    let memory_dir = tempfile::tempdir().unwrap();
    let mut agent = base(provider.clone())
        .with(Memory(memory(memory_dir.path()).init()))
        .build("alice", "s1")
        .await
        .unwrap();
    agent.run("hello").await.unwrap();

    assert_eq!(provider.count(), 1);
}

#[tokio::test]
async fn memory_review_spawns_when_interval_is_due() {
    let provider = Arc::new(RecordingProvider::default());
    let memory_dir = tempfile::tempdir().unwrap();
    let mut agent = base(provider.clone())
        .with(Memory(
            memory(memory_dir.path())
                .init()
                .include_memory_tool()
                .curate_every_turns(1),
        ))
        .build("alice", "s1")
        .await
        .unwrap();
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
async fn curation_guidance_override_reaches_the_curator() {
    let provider = Arc::new(RecordingProvider::default());
    let memory_dir = tempfile::tempdir().unwrap();
    let mut agent = base(provider.clone())
        .with(Memory(
            memory(memory_dir.path())
                .init()
                .include_memory_tool()
                .curate_every_turns(1)
                .curation_guidance("custom curation instructions"),
        ))
        .build("alice", "s1")
        .await
        .unwrap();
    agent.run("hello").await.unwrap();

    for _ in 0..50 {
        if provider.count() >= 2 {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(provider.count(), 2);
    assert_eq!(
        provider.last_request().system.as_deref(),
        Some("custom curation instructions")
    );
}

#[tokio::test]
async fn memory_review_waits_until_interval() {
    let provider = Arc::new(RecordingProvider::default());
    let memory_dir = tempfile::tempdir().unwrap();
    let mut agent = base(provider.clone())
        .with(Memory(
            memory(memory_dir.path())
                .init()
                .include_memory_tool()
                .curate_every_turns(2),
        ))
        .build("alice", "s1")
        .await
        .unwrap();
    agent.run("hello").await.unwrap();

    assert_eq!(provider.count(), 1);
}

#[tokio::test]
async fn memory_review_counts_runs_across_rebuilds() {
    let provider = Arc::new(RecordingProvider::default());
    let memory_dir = tempfile::tempdir().unwrap();
    let composer = base(provider.clone()).with(Memory(
        memory(memory_dir.path())
            .init()
            .include_memory_tool()
            .curate_every_turns(2),
    ));

    let mut first = composer.build("alice", "s1").await.unwrap();
    first.run("one").await.unwrap();
    assert_eq!(provider.count(), 1, "run one: review not due yet");
    let log: Vec<runic_state::SessionEvent> = first.state().events().to_vec();

    let mut second = composer.build("alice", "s1").await.unwrap();
    for event in log {
        second.state_mut().fold_event(event);
    }
    second.run("two").await.unwrap();

    for _ in 0..50 {
        if provider.count() >= 3 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        provider.count(),
        3,
        "a rebuilt agent must still know run one happened — the schedule lives in state"
    );
    assert_eq!(
        second
            .state()
            .get("memory-curator/last-review-run")
            .and_then(|v| v.as_u64()),
        Some(2)
    );
}

struct EchoTool;
#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echoes"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(&self, _a: serde_json::Value, _c: &ToolContext) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("echoed"))
    }
}

#[tokio::test]
async fn registers_custom_tools_and_output_schema() {
    let provider = Arc::new(RecordingProvider::default());
    let mut agent = base(provider.clone())
        .with(Tools(vec![Arc::new(EchoTool)]))
        .output_schema(serde_json::json!({ "type": "object" }))
        .max_turns(2)
        .build("alice", "s1")
        .await
        .unwrap();
    agent.run("hello").await.unwrap();

    let names: Vec<String> = provider
        .last_request()
        .tools
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(names.iter().any(|n| n == "echo"), "custom tool registered");
    assert!(
        names.iter().any(|n| n == "final_answer"),
        "output_schema injects final_answer"
    );
}
