use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{AgentFactory, BoxedAgentFactory, HumanHub, ServeConfig, router};
use runic_substrate::{MemoryArtifactStore, MemorySessionStore, SessionStore};
use runic_types::{ContentBlock, StopReason, TokenUsage};

const TENANT: &str = "alice";

struct EchoProvider {
    reply: &'static str,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl EchoProvider {
    fn new(reply: &'static str) -> Arc<Self> {
        Arc::new(Self {
            reply,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn last_request(&self) -> CompletionRequest {
        self.requests.lock().unwrap().last().unwrap().clone()
    }
}

#[async_trait]
impl Provider for EchoProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(req);
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: self.reply.into(),
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage::default(),
        })
    }
}

struct EchoFactory {
    provider: Arc<EchoProvider>,
    description: &'static str,
}

#[async_trait]
impl AgentFactory for EchoFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        Agent::builder(self.provider.clone(), tenant, session_id)
            .system_prompt("test")
            .build()
    }

    fn describe(&self) -> Option<&str> {
        Some(self.description)
    }
}

struct StatelessEchoFactory {
    provider: Arc<EchoProvider>,
}

#[async_trait]
impl AgentFactory for StatelessEchoFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        Agent::builder(self.provider.clone(), tenant, session_id)
            .system_prompt("test")
            .build()
    }

    fn stateless(&self) -> bool {
        true
    }
}

struct Fixture {
    app: Router,
    coral: Arc<EchoProvider>,
    scout: Arc<EchoProvider>,
    store: Arc<dyn SessionStore>,
}

fn fixture() -> Fixture {
    let coral = EchoProvider::new("from-coral");
    let scout = EchoProvider::new("from-scout");
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let mut agents: HashMap<String, BoxedAgentFactory> = HashMap::new();
    agents.insert(
        "coral".into(),
        Arc::new(EchoFactory {
            provider: coral.clone(),
            description: "support agent",
        }),
    );
    agents.insert(
        "scout".into(),
        Arc::new(EchoFactory {
            provider: scout.clone(),
            description: "research agent",
        }),
    );
    let app = router(ServeConfig {
        session_store: store.clone(),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents,
        human_hub: Arc::new(HumanHub::new()),
        limits: Default::default(),
    });
    Fixture {
        app,
        coral,
        scout,
        store,
    }
}

fn wait_request(thread: &str, agent: Option<&str>, message: &str) -> Request<Body> {
    let mut body = json!({ "message": message });
    if let Some(agent) = agent {
        body["agent"] = json!(agent);
    }
    Request::builder()
        .method("POST")
        .uri(format!("/threads/{thread}/runs/wait"))
        .header("content-type", "application/json")
        .header("x-runic-tenant", TENANT)
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn list_agents_returns_the_registry_sorted() {
    let f = fixture();
    let resp = f
        .app
        .oneshot(
            Request::builder()
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({
            "agents": [
                { "name": "coral", "description": "support agent" },
                { "name": "scout", "description": "research agent" },
            ]
        })
    );
}

#[tokio::test]
async fn run_routes_to_the_named_agent() {
    let f = fixture();
    let resp = f
        .app
        .clone()
        .oneshot(wait_request("t1", Some("coral"), "hi"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["text"], "from-coral");

    let resp = f
        .app
        .oneshot(wait_request("t2", Some("scout"), "hi"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["text"], "from-scout");
}

#[tokio::test]
async fn unknown_agent_is_404() {
    let f = fixture();
    let resp = f
        .app
        .oneshot(wait_request("t1", Some("ghost"), "hi"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "not_found");
    assert!(body["message"].as_str().unwrap().contains("ghost"));
}

#[tokio::test]
async fn missing_agent_on_a_multi_agent_server_is_400_listing_the_roster() {
    let f = fixture();
    let resp = f.app.oneshot(wait_request("t1", None, "hi")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["error"], "bad_request");
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("coral") && message.contains("scout"));
}

#[tokio::test]
async fn missing_agent_on_a_single_agent_server_routes_to_it() {
    let coral = EchoProvider::new("from-coral");
    let app = router(ServeConfig {
        session_store: Arc::new(runic_substrate::MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: runic_serve::single_agent(
            "coral",
            Arc::new(EchoFactory {
                provider: coral,
                description: "support agent",
            }),
        ),
        human_hub: Arc::new(HumanHub::new()),
        limits: Default::default(),
    });
    let resp = app.oneshot(wait_request("t1", None, "hi")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["text"], "from-coral");
}

#[test]
#[should_panic(expected = "at least one agent")]
fn an_empty_roster_refuses_to_serve() {
    runic_serve::AgentRegistry::new(HashMap::new());
}

#[tokio::test]
async fn stateless_agent_is_rebuilt_every_run_and_persists_nothing() {
    let provider = EchoProvider::new("flash-reply");
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let app = router(ServeConfig {
        session_store: store.clone(),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: runic_serve::single_agent(
            "flash",
            Arc::new(StatelessEchoFactory {
                provider: provider.clone(),
            }),
        ),
        human_hub: Arc::new(HumanHub::new()),
        limits: Default::default(),
    });

    let first = app
        .clone()
        .oneshot(wait_request("t1", None, "remember me"))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    let second = app
        .clone()
        .oneshot(wait_request("t1", None, "what did i say?"))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);

    let seen = provider.last_request();
    let all_text: String = seen
        .messages
        .iter()
        .map(|m| m.content.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_text.contains("what did i say?"));
    assert!(!all_text.contains("remember me"));
    assert!(!all_text.contains("flash-reply"));

    let persisted = store
        .read(TENANT, "t1")
        .await
        .map(|events| events.len())
        .unwrap_or(0);
    assert_eq!(persisted, 0);
}

#[tokio::test]
async fn second_agent_sees_the_first_agents_conversation() {
    let f = fixture();
    let resp = f
        .app
        .clone()
        .oneshot(wait_request(
            "shared",
            Some("coral"),
            "remember the launch is friday",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = f
        .app
        .oneshot(wait_request("shared", Some("scout"), "when is the launch?"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["text"], "from-scout");

    let seen = f.scout.last_request();
    let all_text: String = seen
        .messages
        .iter()
        .map(|m| m.content.text_content())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_text.contains("remember the launch is friday"));
    assert!(all_text.contains("from-coral"));
    assert!(all_text.contains("when is the launch?"));
    let _ = &f.coral;
}

#[tokio::test]
async fn run_start_event_records_the_agent() {
    let f = fixture();
    let resp = f
        .app
        .oneshot(wait_request("t1", Some("coral"), "hi"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let events = f.store.read(TENANT, "t1").await.unwrap();
    let agent = events.iter().find_map(|e| match &e.event {
        runic_state::SessionEvent::RunStart { agent, .. } => Some(agent.clone()),
        _ => None,
    });
    assert_eq!(agent.flatten().as_deref(), Some("coral"));
}
