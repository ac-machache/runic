use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::tool::{Tool, ToolContext, ToolResult};
use runic::{Agent, Input, Llm, subagent};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_store::{ArtifactStore, SessionEvent, Store};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};

struct ScriptedProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(req);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Parse("exhausted".into()))
    }
}

fn text(content: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: content.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage::default(),
    }
}

#[tokio::test]
async fn agent_runs_standalone_and_stateless() {
    let provider = ScriptedProvider::new(vec![text("hello there")]);
    let agent = Agent::new(Llm::new(provider, "test-model"));

    let out = agent.run(Input::text("hi")).await.unwrap();
    assert_eq!(out.text, "hello there");
    assert_eq!(out.outcome.total_turns, 1);
}

#[tokio::test]
async fn session_persists_and_hydrates_across_runs() {
    let provider = ScriptedProvider::new(vec![text("my name is Ada"), text("you are Ada")]);
    let store = Store::memory().unwrap();
    let agent = Agent::new(Llm::new(provider.clone(), "test-model"));

    let first = runic::session(("tenant", "session-1"))
        .store(store.clone())
        .invoke(&agent, Input::text("remember my name"))
        .await
        .unwrap();
    assert_eq!(first.text, "my name is Ada");

    let stored = store.sessions().read("tenant", "session-1").await.unwrap();
    assert!(
        !stored.is_empty(),
        "the run's events are appended to the store"
    );

    let second = runic::session(("tenant", "session-1"))
        .store(store.clone())
        .invoke(&agent, Input::text("what is my name"))
        .await
        .unwrap();
    assert_eq!(second.text, "you are Ada");

    let second_request = &provider.requests()[1];
    let texts: Vec<String> = second_request
        .messages
        .iter()
        .map(|m| m.content.text_content())
        .collect();
    assert!(
        texts.iter().any(|t| t == "remember my name"),
        "run 2 hydrated run 1's user message: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "my name is Ada"),
        "run 2 hydrated run 1's assistant reply: {texts:?}"
    );
}

struct Probe(&'static str);

#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "a tool a store handed to the agent"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("ran"))
    }
}

struct ArtifactCounter(Arc<dyn ArtifactStore>);

#[async_trait]
impl Tool for ArtifactCounter {
    fn name(&self) -> &str {
        "count_artifacts"
    }

    fn description(&self) -> &str {
        "count the artifacts the store travels with"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let held = self.0.list(&ctx.user_id, &ctx.session_id).await?.len();
        Ok(ToolResult::ok(held.to_string()))
    }
}

fn call(call_id: &str, name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: call_id.into(),
            name: name.into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: call_id.into(),
            name: name.into(),
            input,
        }],
        usage: TokenUsage::default(),
    }
}

fn delegate_to(agent: &str) -> CompletionResponse {
    call(
        "c1",
        "delegate",
        serde_json::json!({ "agent": agent, "prompt": "dig" }),
    )
}

#[subagent(name = "researcher", description = "digs")]
struct Researcher(Arc<ScriptedProvider>);

impl Researcher {
    async fn agent(&self, _llm: Llm) -> anyhow::Result<Agent> {
        Ok(Agent::new(Llm::new(self.0.clone(), "child-model")))
    }
}

#[tokio::test]
async fn session_persists_a_delegated_run_to_its_own_child_session() {
    let parent = ScriptedProvider::new(vec![delegate_to("researcher"), text("parent done")]);
    let child = ScriptedProvider::new(vec![text("child found it")]);
    let store = Store::memory().unwrap();

    let agent = Agent::new(Llm::new(parent, "main-model")).with(Researcher(child));

    let out = runic::session(("tenant", "t1"))
        .store(store.clone())
        .invoke(&agent, Input::text("go"))
        .await
        .unwrap();
    assert_eq!(out.text, "parent done");

    let parent_log = store.sessions().read("tenant", "t1").await.unwrap();
    let child_session = parent_log
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::DelegationFinished { child_session, .. } => child_session.clone(),
            _ => None,
        })
        .expect("a DelegationFinished carrying the child session id");

    let child_log = store
        .sessions()
        .read("tenant", &child_session)
        .await
        .unwrap();
    let child_texts: Vec<String> = child_log
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::Message { msg, .. } => Some(msg.content.text_content()),
            _ => None,
        })
        .collect();
    assert!(
        child_texts.iter().any(|t| t.contains("child found it")),
        "the child's transcript persisted to its own session: {child_texts:?}"
    );
}

#[tokio::test]
async fn one_session_can_be_answered_by_different_agents() {
    let store = Store::memory().unwrap();

    let first_provider = ScriptedProvider::new(vec![text("noted")]);
    let support = Agent::new(Llm::new(first_provider, "m").instructions("you are support"));

    runic::session(("tenant", "shared-session"))
        .store(store.clone())
        .invoke(&support, Input::text("my order id is 4417"))
        .await
        .unwrap();

    let second_provider = ScriptedProvider::new(vec![text("4417")]);
    let analyst =
        Agent::new(Llm::new(second_provider.clone(), "m").instructions("you are analyst"));

    runic::session(("tenant", "shared-session"))
        .store(store.clone())
        .invoke(&analyst, Input::text("what was the order id"))
        .await
        .unwrap();

    let seen = second_provider.requests();
    let transcript = seen
        .last()
        .expect("the analyst called the model")
        .messages
        .iter()
        .map(|msg| msg.content.text_content())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        transcript.contains("4417"),
        "a different agent on the same session must hydrate the earlier turns:\n{transcript}"
    );
    assert!(
        seen.last().unwrap().system.as_deref() == Some("you are analyst"),
        "…while still using its own instructions"
    );
}

#[tokio::test]
async fn a_store_contributes_only_the_tools_it_was_given() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let agent = Agent::new(Llm::new(provider.clone(), "test-model"));

    // Plain store: no tools travel with it.
    let bare = Store::memory().unwrap();
    runic::session(("tenant", "t1"))
        .store(bare)
        .invoke(&agent, Input::text("go"))
        .await
        .unwrap();
    let offered = provider.requests().last().unwrap().tools.len();
    assert_eq!(offered, 0, "a store with no tools contributes nothing");

    // Same store, a tool handed to it.
    let provider = ScriptedProvider::new(vec![text("done")]);
    let agent = Agent::new(Llm::new(provider.clone(), "test-model"));
    let searchable = Store::memory().unwrap().tool(Probe("search_chats"));
    runic::session(("tenant", "t1"))
        .store(searchable)
        .invoke(&agent, Input::text("go"))
        .await
        .unwrap();

    let names: Vec<String> = provider
        .requests()
        .last()
        .unwrap()
        .tools
        .iter()
        .map(|spec| spec.name.clone())
        .collect();
    assert_eq!(names, vec!["search_chats".to_string()]);
}

#[tokio::test]
async fn a_store_tool_can_read_the_artifacts_it_travels_with() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let agent = Agent::new(Llm::new(provider.clone(), "test-model"));

    let store = Store::memory()
        .unwrap()
        .tool_with(|store| ArtifactCounter(store.artifacts()));
    store
        .artifacts()
        .put(
            "tenant",
            "t1",
            "text/plain",
            runic::store::ArtifactSource::UserUpload,
            b"one",
        )
        .await
        .unwrap();

    runic::session(("tenant", "t1"))
        .store(store)
        .invoke(&agent, Input::text("go"))
        .await
        .unwrap();

    let names: Vec<String> = provider
        .requests()
        .last()
        .unwrap()
        .tools
        .iter()
        .map(|spec| spec.name.clone())
        .collect();
    assert_eq!(names, vec!["count_artifacts".to_string()]);
}

struct Stamp(&'static str);

#[async_trait]
impl runic::hook::WriteHook for Stamp {
    fn name(&self) -> &str {
        self.0
    }
    fn points(&self) -> &'static [runic::hook::HookLifecycle] {
        &[runic::hook::HookLifecycle::BeforeModel]
    }
    async fn before_model(
        &self,
        _state: &mut runic::state::AgentState,
        request: &mut CompletionRequest,
    ) -> runic::hook::HookOutcome {
        request.messages.push(runic_types::Message::user(self.0));
        runic::hook::HookOutcome::Continue
    }
}

#[tokio::test]
async fn a_store_contributes_the_hooks_it_was_given() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let agent = Agent::new(Llm::new(provider.clone(), "test-model"));

    let store = Store::memory()
        .unwrap()
        .hook(Stamp("from-first"))
        .hook(Stamp("from-second"));

    runic::session(("tenant", "t1"))
        .store(store)
        .invoke(&agent, Input::text("go"))
        .await
        .unwrap();

    let stamps: Vec<String> = provider
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .filter_map(|message| match &message.content {
            runic_types::MessageContent::Text(text) if text.starts_with("from-") => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        stamps,
        vec!["from-first".to_string(), "from-second".to_string()],
        "every hook the store carries reaches the loop, in order"
    );
}

#[tokio::test]
async fn a_session_answers_for_its_own_session_without_reaching_for_the_store() {
    let provider = ScriptedProvider::new(vec![text("noted")]);
    let agent = Agent::new(Llm::new(provider, "test-model"));
    let chat = runic::session(("tenant", "t1")).store(Store::memory().unwrap());
    chat.invoke(&agent, Input::text("my order is 4417"))
        .await
        .unwrap();
    chat.set_label(Some("order 4417")).await.unwrap();

    assert_eq!(chat.label().await.unwrap().as_deref(), Some("order 4417"));

    let meta = chat.meta().await.unwrap().expect("the session exists");
    assert_eq!(meta.run_count, 1);
    assert!(meta.event_count > 0, "the meta row aggregates the log");

    let messages = chat.messages().await.unwrap();
    assert_eq!(messages.len(), 2, "the user turn and the reply");
    assert_eq!(messages[0].content.text_content(), "my order is 4417");

    assert!(!chat.events().await.unwrap().is_empty());

    chat.delete().await.unwrap();
    assert!(chat.meta().await.unwrap().is_none(), "the session is gone");
    assert!(chat.events().await.unwrap().is_empty());
}

#[tokio::test]
async fn without_a_store_a_session_has_no_history_and_cannot_be_written_to() {
    let provider = ScriptedProvider::new(vec![text("hello")]);
    let agent = Agent::new(Llm::new(provider, "test-model"));
    let chat = runic::session(("tenant", "t1"));

    assert_eq!(
        chat.invoke(&agent, Input::text("hi")).await.unwrap().text,
        "hello"
    );

    assert!(chat.meta().await.unwrap().is_none());
    assert!(chat.label().await.unwrap().is_none());
    assert!(chat.events().await.unwrap().is_empty());
    assert!(chat.messages().await.unwrap().is_empty());

    assert!(
        chat.set_label(Some("x")).await.is_err(),
        "a write with nowhere to go must fail, never silently no-op"
    );
    assert!(chat.delete().await.is_err());
}

#[derive(Debug)]
struct Collect(Arc<Mutex<Vec<String>>>);

impl runic::state::Emitter for Collect {
    fn emit(&self, event: runic::state::AgentEvent) {
        let kind = match event {
            runic::state::AgentEvent::RunEnd { .. } => "RunEnd".to_string(),
            runic::state::AgentEvent::Persisted { status, .. } => format!("Persisted:{status:?}"),
            _ => return,
        };
        self.0.lock().unwrap().push(kind);
    }
}

#[tokio::test]
async fn persisted_lands_after_the_run_ended_because_done_is_not_durable() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let agent = Agent::new(Llm::new(provider, "test-model"));
    let seen = Arc::new(Mutex::new(Vec::new()));

    runic::session(("tenant", "t1"))
        .store(Store::memory().unwrap())
        .invoke(
            &agent,
            Input::text("go").events(Arc::new(Collect(seen.clone()))),
        )
        .await
        .unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec!["RunEnd".to_string(), "Persisted:Flushed".to_string(),],
        "the run ends, then the log catches up — two separate facts, in that order"
    );
}

#[tokio::test]
async fn a_storeless_run_never_claims_to_have_persisted() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let agent = Agent::new(Llm::new(provider, "test-model"));
    let seen = Arc::new(Mutex::new(Vec::new()));

    runic::session(("tenant", "t1"))
        .invoke(
            &agent,
            Input::text("go").events(Arc::new(Collect(seen.clone()))),
        )
        .await
        .unwrap();

    assert_eq!(*seen.lock().unwrap(), vec!["RunEnd".to_string()]);
}

#[tokio::test]
async fn inline_bytes_are_stored_once_and_the_log_keeps_only_the_pointer() {
    let provider = ScriptedProvider::new(vec![text("seen")]);
    let agent = Agent::new(Llm::new(provider.clone(), "test-model"));
    let store = Store::memory().unwrap();
    let chat = runic::session(("tenant", "t1")).store(store.clone());

    chat.invoke(
        &agent,
        Input::text("what is this").image("image/png", b"\x89PNG"),
    )
    .await
    .unwrap();

    let held = store.artifacts().list("tenant", "t1").await.unwrap();
    assert_eq!(held.len(), 1, "the bytes went to the artifact store");
    assert_eq!(held[0].mime_type, "image/png");
    assert_eq!(held[0].size, 4);

    let logged = chat.messages().await.unwrap();
    let blocks = match &logged[0].content {
        runic_types::MessageContent::Blocks(blocks) => blocks,
        other => panic!("expected blocks, got {other:?}"),
    };
    assert!(
        matches!(&blocks[1], ContentBlock::Image { source, .. } if source.stored() == Some(held[0].id.as_str())),
        "the log carries a pointer, never the bytes: {:?}",
        blocks[1]
    );
}

#[tokio::test]
async fn a_stored_pointer_from_another_session_is_refused() {
    let provider = ScriptedProvider::new(vec![text("never")]);
    let agent = Agent::new(Llm::new(provider, "test-model"));
    let store = Store::memory().unwrap();
    let theirs = store
        .artifacts()
        .put(
            "tenant",
            "someone-else",
            "image/png",
            runic_store::artifacts::ArtifactSource::UserUpload,
            b"\x89PNG",
        )
        .await
        .unwrap();

    let outcome = runic::session(("tenant", "mine"))
        .store(store)
        .invoke(&agent, Input::new().artifact(&theirs.id, "image/png"))
        .await;

    let Err(error) = outcome else {
        panic!("an artifact from another session must not be readable");
    };
    assert!(
        error
            .to_string()
            .contains("does not belong to this session"),
        "{error}"
    );
}

#[tokio::test]
async fn a_storeless_run_keeps_its_bytes_inline() {
    let provider = ScriptedProvider::new(vec![text("ok")]);
    let agent = Agent::new(Llm::new(provider.clone(), "test-model"));

    runic::session(("tenant", "t1"))
        .invoke(&agent, Input::text("look").image("image/png", b"\x89PNG"))
        .await
        .unwrap();

    let sent = provider.requests();
    let blocks = match &sent[0].messages[0].content {
        runic_types::MessageContent::Blocks(blocks) => blocks,
        other => panic!("expected blocks, got {other:?}"),
    };
    assert!(
        matches!(&blocks[1], ContentBlock::Image { source, .. } if source.inline().is_some()),
        "with no store there is nowhere to put bytes, so they ride along: {:?}",
        blocks[1]
    );
}
