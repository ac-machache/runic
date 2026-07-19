use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::Llm;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_tool::{Tool, ToolContext, ToolResult};
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

    fn last_request(&self) -> CompletionRequest {
        self.requests.lock().unwrap().last().unwrap().clone()
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
            .ok_or_else(|| ProviderError::Parse("scripted provider exhausted".into()))
    }
}

fn text(t: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: t.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage {
            input_tokens: 3,
            output_tokens: 5,
            ..Default::default()
        },
    }
}

fn call(name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: name.into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: name.into(),
            input,
        }],
        usage: TokenUsage::default(),
    }
}

struct Recorder(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Tool for Recorder {
    fn name(&self) -> &str {
        "record"
    }
    fn description(&self) -> &str {
        "records a fact"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        self.0.lock().unwrap().push(args.to_string());
        Ok(ToolResult::ok("stored"))
    }
}

#[tokio::test]
async fn run_returns_the_final_text_and_usage() {
    let provider = ScriptedProvider::new(vec![text("curated 3 facts")]);
    let out = Llm::new(provider, "m")
        .instructions("review the transcript")
        .run("User: hi\nAssistant: hello")
        .await
        .unwrap();
    assert_eq!(out.text, "curated 3 facts");
    assert_eq!(out.usage.output_tokens, 5);
}

#[tokio::test]
async fn tools_execute_in_a_mini_loop() {
    let provider = ScriptedProvider::new(vec![
        call("record", serde_json::json!({ "fact": "launch is friday" })),
        text("done"),
    ]);
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let out = Llm::new(provider, "m")
        .instructions("curate")
        .tool(Recorder(recorded.clone()))
        .run("transcript")
        .await
        .unwrap();

    assert_eq!(out.text, "done");
    let recorded = recorded.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].contains("launch is friday"));
}

#[tokio::test]
async fn config_lands_on_the_request() {
    let provider = ScriptedProvider::new(vec![text("ok")]);
    Llm::new(provider.clone(), "mistral-large-latest")
        .instructions("be terse")
        .temperature(0.2)
        .max_tokens(512)
        .thinking(true)
        .run("hi")
        .await
        .unwrap();

    let req = provider.last_request();
    assert_eq!(req.model, "mistral-large-latest");
    assert_eq!(req.temperature, 0.2);
    assert_eq!(req.max_tokens, 512);
    assert_eq!(req.system.as_deref(), Some("be terse"));
    assert!(req.thinking.as_ref().is_some_and(|t| t.enabled));
}

#[tokio::test]
async fn structured_output_parses_into_a_type() {
    #[derive(serde::Deserialize)]
    struct Verdict {
        accepted: bool,
    }

    let provider = ScriptedProvider::new(vec![call(
        "final_answer",
        serde_json::json!({ "accepted": true }),
    )]);
    let out = Llm::new(provider, "m")
        .output_schema(serde_json::json!({
            "type": "object",
            "properties": { "accepted": { "type": "boolean" } },
            "required": ["accepted"]
        }))
        .run("judge this")
        .await
        .unwrap();

    let verdict: Verdict = out.parse().unwrap();
    assert!(verdict.accepted);
}

#[tokio::test]
async fn parse_without_structured_output_errors() {
    let provider = ScriptedProvider::new(vec![text("plain")]);
    let out = Llm::new(provider, "m").run("hi").await.unwrap();
    assert!(out.parse::<serde_json::Value>().is_err());
}

#[tokio::test]
async fn stream_delivers_events_and_the_final_output() {
    let provider = ScriptedProvider::new(vec![text("streamed reply")]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let out = Llm::new(provider, "m").stream("hi", tx).await.unwrap();

    assert_eq!(out.text, "streamed reply");
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(!events.is_empty());
}

#[tokio::test]
async fn a_configured_llm_is_reusable_across_runs() {
    let provider = ScriptedProvider::new(vec![text("one"), text("two")]);
    let llm = Llm::new(provider, "m").instructions("terse");
    assert_eq!(llm.run("a").await.unwrap().text, "one");
    assert_eq!(llm.run("b").await.unwrap().text, "two");
}
