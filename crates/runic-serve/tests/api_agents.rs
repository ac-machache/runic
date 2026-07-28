use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use runic::ability::ability;
use runic::composer::Agent;
use runic::composer::{Composer, Runtime};
use runic::subagent::Subagent;
use runic::{Llm, agent};
use runic_agent::Runner;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::routes::agents::{
    AbilityOverview, AgentOverview, SkillOverview, SubagentOverview, ToolOverview,
};
use runic_serve::{AgentFactory, BoxedAgentFactory, ServeConfig, router};
use runic_substrate::{MemoryArtifactStore, MemorySessionStore, SessionStore};
use runic_tool::{Tool, ToolContext, ToolResult};
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
    async fn build(&self, tenant: &str, session_id: &str) -> anyhow::Result<Runner> {
        Ok(Runner::builder(self.provider.clone(), tenant, session_id)
            .system_prompt("test")
            .build())
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
    async fn build(&self, tenant: &str, session_id: &str) -> anyhow::Result<Runner> {
        Ok(Runner::builder(self.provider.clone(), tenant, session_id)
            .system_prompt("test")
            .build())
    }

    fn stateless(&self) -> bool {
        true
    }
}

struct AddTool;

#[async_trait]
impl Tool for AddTool {
    fn name(&self) -> &str {
        "add"
    }
    fn description(&self) -> &str {
        "add two numbers"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("2"))
    }
}

struct RefundTool;

#[async_trait]
impl Tool for RefundTool {
    fn name(&self) -> &str {
        "refund"
    }
    fn description(&self) -> &str {
        "issue a refund"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("refunded"))
    }
}

async fn billing_skills() -> Arc<runic::skills::SkillSet> {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("dispute");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: dispute\ndescription: how to handle a billing dispute\n---\nBe polite.",
    )
    .unwrap();
    Arc::new(runic::skills::SkillSet::load_dir("billing", dir.path()).await)
}

async fn rich_composer(provider: Arc<EchoProvider>) -> Composer {
    Composer::new(
        runic::composer::Agent::new(Llm::new(provider, "test-model").instructions("root"))
            .with(
                ability("core")
                    .describe("always-on core tools")
                    .tool(AddTool),
            )
            .with(
                ability("billing")
                    .describe("invoices and refunds")
                    .deferred()
                    .tool(RefundTool)
                    .skills(billing_skills().await)
                    .subagent(Subagent::new(
                        "billing-worker",
                        "handles billing disputes",
                        Agent::new(
                            Llm::new(EchoProvider::new("child done"), "child-model")
                                .instructions("you are a billing worker")
                                .max_turns(3),
                        ),
                    )),
            ),
        Runtime::new(),
    )
}

fn map_overview(name: &str, views: Vec<runic::composer::AbilityView>) -> AgentOverview {
    AgentOverview {
        name: name.to_string(),
        model: None,
        max_turns: None,
        abilities: views
            .into_iter()
            .map(|view| AbilityOverview {
                id: view.id,
                name: view.name,
                description: view.description,
                deferred: view.deferred,
                activated: view.activated,
                tools: view
                    .tools
                    .into_iter()
                    .map(|spec| ToolOverview {
                        name: spec.name,
                        description: spec.description,
                        parameters: spec.parameters,
                    })
                    .collect(),
                skills: view
                    .skills
                    .into_iter()
                    .map(|skill| SkillOverview {
                        id: skill.id,
                        description: skill.description,
                    })
                    .collect(),
                subagents: view
                    .subagents
                    .into_iter()
                    .map(|subagent| SubagentOverview {
                        name: subagent.name,
                        description: subagent.description,
                    })
                    .collect(),
                hooks: view.hooks,
            })
            .collect(),
    }
}

struct RichFactory {
    provider: Arc<EchoProvider>,
}

#[async_trait]
impl AgentFactory for RichFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> anyhow::Result<Runner> {
        Ok(rich_composer(self.provider.clone())
            .await
            .build(tenant, session_id)
            .await?)
    }

    async fn overview(&self, tenant: &str, session_id: &str) -> Option<AgentOverview> {
        let views = rich_composer(self.provider.clone())
            .await
            .describe(tenant, session_id)
            .await
            .ok()?;
        Some(map_overview("rich", views))
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
        limits: Default::default(),
        workers: None,
        broker: None,
        nudge: None,
        identity: None,
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
        limits: Default::default(),
        workers: None,
        broker: None,
        nudge: None,
        identity: None,
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
        limits: Default::default(),
        workers: None,
        broker: None,
        nudge: None,
        identity: None,
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
        runic_substrate::SessionEvent::RunStart { agent, .. } => Some(agent.clone()),
        _ => None,
    });
    assert_eq!(agent.flatten().as_deref(), Some("coral"));
}

#[tokio::test]
async fn serve_config_builder_defaults_the_optional_infra() {
    let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let config = ServeConfig::new(store, Arc::new(MemoryArtifactStore::new())).factory(
        "coral",
        Arc::new(EchoFactory {
            provider: EchoProvider::new("hi"),
            description: "support",
        }),
    );
    assert!(config.transcriber.is_none());
    assert!(config.workers.is_none());
    assert!(config.broker.is_none());
    assert!(config.nudge.is_none());
    assert!(config.identity.is_none());

    let app = router(config);
    let resp = app
        .oneshot(wait_request("t1", Some("coral"), "hi"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["text"], "hi");
}

fn get_agent(name: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/agents/{name}"))
        .body(Body::empty())
        .unwrap()
}

fn overview_app() -> Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agents: runic_serve::single_agent(
            "rich",
            Arc::new(RichFactory {
                provider: EchoProvider::new("unused"),
            }),
        ),
        limits: Default::default(),
        workers: None,
        broker: None,
        nudge: None,
        identity: None,
    })
}

#[tokio::test]
async fn overview_groups_tools_skills_and_subagents_by_ability() {
    let resp = overview_app().oneshot(get_agent("rich")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    let abilities = body["abilities"].as_array().unwrap();
    assert_eq!(abilities.len(), 2);

    let core = abilities.iter().find(|a| a["name"] == "core").unwrap();
    assert_eq!(core["deferred"], false);
    assert_eq!(core["activated"], true);
    assert_eq!(core["tools"][0]["name"], "add");
    assert!(core["skills"].as_array().unwrap().is_empty());
    assert!(core["subagents"].as_array().unwrap().is_empty());

    let billing = abilities.iter().find(|a| a["name"] == "billing").unwrap();
    assert_eq!(billing["id"], "billing");
    assert_eq!(billing["description"], "invoices and refunds");
    assert_eq!(billing["deferred"], true);
    assert_eq!(billing["activated"], false);
    assert_eq!(billing["tools"][0]["name"], "refund");
    assert_eq!(billing["skills"][0]["id"], "billing:dispute");
    assert_eq!(
        billing["skills"][0]["description"],
        "how to handle a billing dispute"
    );
    assert_eq!(billing["subagents"][0]["name"], "billing-worker");
    assert_eq!(
        billing["subagents"][0]["description"],
        "handles billing disputes"
    );
}

#[tokio::test]
async fn overview_falls_back_to_a_flat_view_for_a_non_composer_factory() {
    let f = fixture();
    let resp = f.app.oneshot(get_agent("coral")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    let abilities = body["abilities"].as_array().unwrap();
    assert_eq!(abilities.len(), 1);
    assert_eq!(abilities[0]["name"], "agent");
    assert!(abilities[0]["id"].is_null());
    assert!(abilities[0]["tools"].as_array().unwrap().is_empty());
    assert!(abilities[0]["hooks"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn overview_of_an_unknown_agent_is_404() {
    let f = fixture();
    let resp = f.app.oneshot(get_agent("ghost")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[agent(name = "support", description = "customer support")]
struct Support {
    provider: Arc<EchoProvider>,
}

impl Support {
    async fn agent(&self) -> anyhow::Result<Agent> {
        Ok(Agent::new(
            Llm::new(self.provider.clone(), "test-model").instructions("you are support"),
        )
        .with(ability("kit").tool(RefundTool)))
    }
}

#[tokio::test]
async fn an_agent_macro_type_is_served_under_the_name_it_declares() {
    let config = ServeConfig::new(
        Arc::new(MemorySessionStore::new()) as Arc<dyn SessionStore>,
        Arc::new(MemoryArtifactStore::new()),
    )
    .agent(Support {
        provider: EchoProvider::new("supported"),
    });
    let app = router(config);

    let resp = app
        .clone()
        .oneshot(wait_request("t1", Some("support"), "hi"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the name came off the AgentDef, not a hand-typed registry key"
    );

    let listed = body_json(
        app.oneshot(
            Request::builder()
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await;
    let support = listed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "support")
        .expect("registered under its declared name");
    assert_eq!(
        support["description"], "customer support",
        "and describe() came off the attribute too"
    );
}
