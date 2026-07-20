use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::Llm;
use runic::StructuredOutput;
use runic::composer::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
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
            .ok_or_else(|| ProviderError::Parse("exhausted".into()))
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

#[derive(Debug, PartialEq, serde::Deserialize, schemars::JsonSchema)]
struct Verdict {
    answer: String,
    confidence: f64,
}

#[tokio::test]
async fn a_typed_output_round_trips_through_final_answer() {
    let provider = ScriptedProvider::new(vec![call(
        "final_answer",
        serde_json::json!({ "answer": "yes", "confidence": 0.9 }),
    )]);
    let mut agent = Agent::new(Llm::new(provider.clone(), "test-model").instructions("judge"))
        .output::<Verdict>()
        .build("alice", "s1")
        .await
        .unwrap();

    let outcome = agent.run("is water wet?").await.unwrap();

    let verdict: Verdict = outcome.output_as().unwrap();
    assert_eq!(
        verdict,
        Verdict {
            answer: "yes".into(),
            confidence: 0.9
        }
    );

    let final_answer = provider
        .last_request()
        .tools
        .into_iter()
        .find(|tool| tool.name == "final_answer")
        .expect("final_answer tool registered");
    let schema = final_answer.input_schema;
    assert!(schema.get("$schema").is_none());
    assert!(schema.get("title").is_none());
    assert_eq!(schema["properties"]["answer"]["type"], "string");
    assert_eq!(schema["properties"]["confidence"]["type"], "number");
}

#[tokio::test]
async fn output_as_reports_a_missing_structured_result() {
    let provider = ScriptedProvider::new(vec![CompletionResponse {
        content: vec![ContentBlock::Text {
            text: "plain text".into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage::default(),
    }]);
    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .output::<Verdict>()
        .build("alice", "s1")
        .await
        .unwrap();

    let outcome = agent.run("go").await.unwrap();

    let error = outcome.output_as::<Verdict>().unwrap_err();
    assert!(error.to_string().contains("no structured output"));
}
