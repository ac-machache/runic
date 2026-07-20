use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use runic::hooks::TaskReminder;
use runic_agent::{Session, TasksSnapshot};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_state::SessionEvent;
use runic_state::{TaskStatus, ThreadStats};
use runic_subagent::{DelegateTool, Subagent, SubagentBuilder, SubagentReq};
use runic_tool::{Tool, ToolContext};
use runic_types::{ContentBlock, Message, StopReason, TokenUsage, ToolCall};

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
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Parse("scripted provider exhausted".into()))
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

fn delegate_background_response(id: &str) -> CompletionResponse {
    let input = serde_json::json!({
        "action": "delegate",
        "agent": "scout",
        "prompt": "research competitors",
        "background": true,
    });
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: id.into(),
            name: "delegate".into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "delegate".into(),
            input,
        }],
        usage: TokenUsage::default(),
    }
}

struct StubBuilder;

#[async_trait]
impl SubagentBuilder for StubBuilder {
    async fn provider(&self, _req: &SubagentReq<'_>) -> Arc<dyn Provider> {
        ScriptedProvider::new(vec![text_response("found 3 competitors")])
    }

    fn default_model(&self, _req: &SubagentReq<'_>) -> String {
        String::new()
    }
}

fn scout_roster() -> Vec<Subagent> {
    vec![Subagent::new("scout", "research").prompt("you research")]
}

async fn wait_for_finish(rx: &mut tokio::sync::broadcast::Receiver<Arc<SessionEvent>>) {
    loop {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Ok(ev)) if matches!(ev.as_ref(), SessionEvent::TaskFinished { .. }) => return,
            Ok(Ok(_)) => continue,
            other => panic!("background task never finished: {other:?}"),
        }
    }
}

#[tokio::test]
async fn background_delegation_lands_in_state_stats_and_the_next_model_call() {
    let provider = ScriptedProvider::new(vec![
        delegate_background_response("t1"),
        text_response("spawned, moving on"),
        text_response("done"),
    ]);
    let delegate = DelegateTool::with_builder(scout_roster(), Arc::new(StubBuilder));
    let mut agent = Session::builder(provider.clone(), "u1", "s1")
        .system_prompt("sys")
        .tool(Arc::new(delegate))
        .write_hook(Arc::new(TaskReminder::new()))
        .build();
    let (tx, mut rx) = tokio::sync::broadcast::channel(64);
    agent.state_mut().set_events_tx(tx);

    agent
        .run_message(Message::user("go research"))
        .await
        .unwrap();
    wait_for_finish(&mut rx).await;

    agent.run_message(Message::user("and now?")).await.unwrap();

    let record = agent
        .state()
        .tasks()
        .values()
        .next()
        .expect("task record folded into state");
    assert_eq!(record.agent, "scout");
    assert_eq!(record.status, TaskStatus::Completed);
    assert_eq!(record.result.as_deref(), Some("found 3 competitors"));
    let notified_key = format!("task-reminder/notified/{}", record.task_id);
    assert_eq!(
        agent.state().get(&notified_key),
        Some(&serde_json::json!(true))
    );

    let stats: &ThreadStats = agent.state().stats();
    assert_eq!(stats.tasks_spawned, 1);
    assert_eq!(stats.tasks_finished, 1);
    assert_eq!(stats.tasks_failed, 0);

    let last = provider.requests().last().unwrap().clone();
    let all_text: String = last
        .messages
        .iter()
        .map(|m| m.content.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_text.contains("<system-reminder>"));
    assert!(all_text.contains("found 3 competitors"));
}

#[tokio::test]
async fn background_delegation_emits_a_navigable_edge() {
    let provider = ScriptedProvider::new(vec![
        delegate_background_response("t1"),
        text_response("spawned, moving on"),
        text_response("done"),
    ]);
    let delegate = DelegateTool::with_builder(scout_roster(), Arc::new(StubBuilder));
    let mut agent = Session::builder(provider.clone(), "u1", "s1")
        .system_prompt("sys")
        .tool(Arc::new(delegate))
        .build();
    let (tx, mut rx) = tokio::sync::broadcast::channel(64);
    agent.state_mut().set_events_tx(tx);

    agent
        .run_message(Message::user("go research"))
        .await
        .unwrap();
    wait_for_finish(&mut rx).await;

    agent.run_message(Message::user("and now?")).await.unwrap();

    let events = agent.state().events();
    let started = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::DelegationStarted {
                agent,
                mode,
                call_id,
                turn,
                ..
            } => Some((agent.clone(), *mode, call_id.clone(), *turn)),
            _ => None,
        })
        .expect("background delegation start edge");
    assert_eq!(started.0, "scout");
    assert_eq!(started.1, runic_state::DelegationMode::Background);
    assert_eq!(started.2, "t1");
    assert_eq!(started.3, 1);

    let finished = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::DelegationFinished {
                agent,
                status,
                call_id,
                ..
            } => Some((agent.clone(), status.clone(), call_id.clone())),
            _ => None,
        })
        .expect("background delegation finish edge");
    assert_eq!(finished.0, "scout");
    assert_eq!(finished.1, runic_state::DelegationStatus::Ok);
    assert_eq!(finished.2, "t1");

    let stats: &ThreadStats = agent.state().stats();
    assert_eq!(stats.delegations, 1);
}

#[tokio::test]
async fn check_result_answers_from_the_durable_view_after_a_rebuild() {
    let rebuilt_view = {
        let provider = ScriptedProvider::new(vec![
            delegate_background_response("t1"),
            text_response("spawned"),
            text_response("later"),
        ]);
        let delegate = DelegateTool::with_builder(scout_roster(), Arc::new(StubBuilder));
        let mut agent = Session::builder(provider, "u1", "s1")
            .system_prompt("sys")
            .tool(Arc::new(delegate))
            .build();
        let (tx, mut rx) = tokio::sync::broadcast::channel(64);
        agent.state_mut().set_events_tx(tx);
        agent.run_message(Message::user("go")).await.unwrap();
        wait_for_finish(&mut rx).await;
        agent.run_message(Message::user("sync")).await.unwrap();
        agent.state().tasks().clone()
    };
    assert_eq!(rebuilt_view.len(), 1);
    let task_id = rebuilt_view.keys().next().unwrap().clone();

    let fresh_delegate = DelegateTool::with_builder(scout_roster(), Arc::new(StubBuilder));
    let mut ctx = ToolContext::new("u1", "s1", "r9");
    ctx.insert(TasksSnapshot(Arc::new(rebuilt_view)));

    let result = fresh_delegate
        .execute(
            serde_json::json!({ "action": "check_result", "task_id": task_id }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error());
    assert_eq!(result.text(), "found 3 competitors");
}
