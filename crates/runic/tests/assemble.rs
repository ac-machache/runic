use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::{Delegation, ability, ask_user, basics, search_chats, weather, web_fetch};
use runic::composer::Agent;
use runic::subagent::Subagent;
use runic::{Compaction, Llm};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_skills::SkillSet;
use runic_substrate::sessions_memory;
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, StopReason, TokenUsage};

fn child_agent(prompt: &str) -> Agent {
    Agent::new(
        Llm::new(
            Arc::new(RecordingProvider {
                requests: Mutex::new(Vec::new()),
            }),
            "child-model",
        )
        .instructions(prompt),
    )
}

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

fn base(provider: Arc<dyn Provider>) -> Agent {
    Agent::new(Llm::new(provider, "model-a").instructions("core instructions"))
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

#[tokio::test]
async fn composes_prompt_sections_in_order() {
    let provider = Arc::new(RecordingProvider::default());
    let skill_dir = tempfile::tempdir().unwrap();

    write_skill(skill_dir.path(), "review", "review", "reviews code").await;

    let agent = base(provider)
        .with(ability("skills").skills(Arc::new(SkillSet::load_dir("", skill_dir.path()).await)))
        .with(Delegation::new([Subagent::new(
            "researcher",
            "researches",
            child_agent("Act carefully."),
        )]))
        .build("alice", "s1")
        .await
        .unwrap();
    let system = &agent.state().system_prompt;

    let instructions = system.find("core instructions").unwrap();
    let skills = system.find("<available-skills>").unwrap();
    let subagents = system.find("<subagents>").unwrap();

    assert!(instructions < skills);
    assert!(skills < subagents);
}

#[tokio::test]
async fn delegation_voice_flows_through_the_composer() {
    let provider = Arc::new(RecordingProvider::default());
    let agent = base(provider)
        .with(
            Delegation::new([Subagent::new(
                "researcher",
                "researches",
                child_agent("dig"),
            )])
            .tag("team")
            .intro("Hand self-contained work to your team:")
            .tool_name("dispatch")
            .tool_description("Send a teammate a task."),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    let system = &agent.state().system_prompt;
    assert!(system.contains("<team>"));
    assert!(system.contains("Hand self-contained work to your team:"));
    assert!(system.contains("- researcher: researches"));
    assert!(!system.contains("<subagents>"));

    let specs = agent.tool_specs();
    let dispatch = specs.iter().find(|s| s.name == "dispatch").unwrap();
    assert_eq!(dispatch.description, "Send a teammate a task.");
    assert!(!specs.iter().any(|s| s.name == "delegate"));
}

#[tokio::test]
async fn registers_enabled_tool_surfaces() {
    let provider = Arc::new(RecordingProvider::default());
    let skill_dir = tempfile::tempdir().unwrap();

    write_skill(skill_dir.path(), "review", "review", "reviews code").await;

    let mut agent = base(provider.clone())
        .with(basics())
        .with(ask_user())
        .with(web_fetch())
        .with(weather())
        .with(ability("skills").skills(Arc::new(SkillSet::load_dir("", skill_dir.path()).await)))
        .with(Delegation::new([Subagent::new(
            "researcher",
            "researches",
            child_agent("Act carefully."),
        )]))
        .with(search_chats(sessions_memory().store()))
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
        "Questionnaire",
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
        "escalate_to_human",
        "ask_user",
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
        .hook(
            Compaction::new(Llm::new(provider.clone(), "model-a"))
                .max_context_tokens(12)
                .keep_recent(2),
        )
        .build("alice", "s1")
        .await
        .unwrap();
    let old = [
        runic_types::Message::user("x".repeat(40)),
        runic_types::Message::assistant("y".repeat(40)),
        runic_types::Message::user("recent question"),
    ];
    for msg in old {
        agent.state_mut().emit(runic_state::AgentEvent::Message {
            run_id: "r0".into(),
            msg,
            at: chrono::Utc::now(),
        });
    }

    let (cap_tx, mut cap_rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .state_mut()
        .set_emitter(Some(Arc::new(runic_agent::ChannelEmitter(cap_tx))));

    agent.run("final question").await.unwrap();

    let mut captured: Vec<runic_state::AgentEvent> = Vec::new();
    while let Ok(ev) = cap_rx.try_recv() {
        captured.push(ev);
    }

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
        captured
            .iter()
            .any(|e| matches!(e, runic_state::AgentEvent::StateSnapshot { .. }))
    );
}

#[tokio::test]
async fn summary_guidance_override_reaches_the_summarizer() {
    let provider = Arc::new(RecordingProvider::default());
    let mut agent = base(provider.clone())
        .hook(
            Compaction::new(Llm::new(provider.clone(), "model-a"))
                .max_context_tokens(12)
                .keep_recent(2)
                .summary_guidance("custom summary instructions"),
        )
        .build("alice", "s1")
        .await
        .unwrap();
    for msg in [
        runic_types::Message::user("x".repeat(40)),
        runic_types::Message::assistant("y".repeat(40)),
        runic_types::Message::user("recent question"),
    ] {
        agent.state_mut().emit(runic_state::AgentEvent::Message {
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
        .hook(
            Compaction::new(Llm::new(provider.clone(), "model-a"))
                .max_context_tokens(12)
                .keep_recent(2),
        )
        .build("alice", "s1")
        .await
        .unwrap();

    let state = agent.state_mut();
    state.emit(runic_state::AgentEvent::TaskSpawned {
        run_id: "r0".into(),
        task_id: "t-done".into(),
        agent: "scout".into(),
        prompt: "dig".into(),
        child_session: None,
        at: chrono::Utc::now(),
    });
    state.emit(runic_state::AgentEvent::TaskFinished {
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
        state.emit(runic_state::AgentEvent::Message {
            run_id: "r0".into(),
            msg,
            at: chrono::Utc::now(),
        });
    }

    let (cap_tx, mut cap_rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .state_mut()
        .set_emitter(Some(Arc::new(runic_agent::ChannelEmitter(cap_tx))));

    agent.run("final question").await.unwrap();

    let mut captured: Vec<runic_state::AgentEvent> = Vec::new();
    while let Ok(ev) = cap_rx.try_recv() {
        captured.push(ev);
    }

    assert!(
        captured
            .iter()
            .any(|e| matches!(e, runic_state::AgentEvent::StateSnapshot { .. }))
    );
    assert!(
        agent.state().get("task-reminder/notified/t-done").is_none(),
        "notified key of a compacted-away task must not survive the snapshot"
    );
    assert_eq!(agent.state().get("keep/me"), Some(&serde_json::json!(1)));
    assert!(agent.state().tasks().is_empty());
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
    let mut agent = Agent::new(
        Llm::new(provider.clone(), "model-a")
            .instructions("core instructions")
            .max_turns(2),
    )
    .with(ability("echo").tool(EchoTool))
    .output_schema(serde_json::json!({ "type": "object" }))
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
