mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use runic::ability::ability;
use runic::subagent::Subagent;
use runic::{Agent, Llm, agent};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{HostedAgents, router};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, StopReason, TokenUsage};

use common::Harness;

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

async fn rich_agent(provider: Arc<EchoProvider>) -> Agent {
    Agent::new(Llm::new(provider, "test-model").instructions("root"))
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
        )
}

struct Fixture {
    app: Router,
    scout: Arc<EchoProvider>,
}

fn fixture(h: &Harness) -> Fixture {
    let coral = EchoProvider::new("from-coral");
    let scout = EchoProvider::new("from-scout");
    let app = router(
        h.config()
            .agent(
                "coral",
                HostedAgents::new(common::agent(coral)).describe("support agent"),
            )
            .agent(
                "scout",
                HostedAgents::new(common::agent(scout.clone())).describe("research agent"),
            ),
    );
    Fixture { app, scout }
}

fn get_agent(name: &str, tenant: &str) -> Request<Body> {
    common::get(&format!("/agents/{name}"), tenant)
}

#[tokio::test]
async fn list_agents_returns_the_registry_sorted() {
    let Some(h) = common::harness().await else {
        return;
    };
    let f = fixture(&h);
    let resp = f
        .app
        .oneshot(common::get("/agents", &h.tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
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
    let Some(h) = common::harness().await else {
        return;
    };
    let f = fixture(&h);
    let t1 = common::uid("t");
    let resp = f
        .app
        .clone()
        .oneshot(common::wait_request_agent(
            &t1,
            &h.tenant,
            Some("coral"),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(common::body_json(resp).await["text"], "from-coral");

    let t2 = common::uid("t");
    let resp = f
        .app
        .oneshot(common::wait_request_agent(
            &t2,
            &h.tenant,
            Some("scout"),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(common::body_json(resp).await["text"], "from-scout");
}

#[tokio::test]
async fn unknown_agent_is_404() {
    let Some(h) = common::harness().await else {
        return;
    };
    let f = fixture(&h);
    let session = common::uid("t");
    let resp = f
        .app
        .oneshot(common::wait_request_agent(
            &session,
            &h.tenant,
            Some("ghost"),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"], "not_found");
    assert!(body["message"].as_str().unwrap().contains("ghost"));
}

#[tokio::test]
async fn missing_agent_on_a_multi_agent_server_is_400_listing_the_roster() {
    let Some(h) = common::harness().await else {
        return;
    };
    let f = fixture(&h);
    let session = common::uid("t");
    let resp = f
        .app
        .oneshot(common::wait_request(&session, &h.tenant, "hi"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = common::body_json(resp).await;
    assert_eq!(body["error"], "bad_request");
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("coral") && message.contains("scout"));
}

#[tokio::test]
async fn missing_agent_on_a_single_agent_server_routes_to_it() {
    let Some(h) = common::harness().await else {
        return;
    };
    let coral = EchoProvider::new("from-coral");
    let app = h.single_router(common::agent(coral));
    let session = common::uid("t");
    let resp = app
        .oneshot(common::wait_request(&session, &h.tenant, "hi"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(common::body_json(resp).await["text"], "from-coral");
}

#[test]
#[should_panic(expected = "at least one agent")]
fn an_empty_roster_refuses_to_serve() {
    runic_serve::AgentRegistry::new(std::collections::HashMap::new());
}

#[tokio::test]
async fn second_agent_sees_the_first_agents_conversation() {
    let Some(h) = common::harness().await else {
        return;
    };
    let f = fixture(&h);
    let shared = common::uid("shared");
    let resp = f
        .app
        .clone()
        .oneshot(common::wait_request_agent(
            &shared,
            &h.tenant,
            Some("coral"),
            "remember the launch is friday",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = f
        .app
        .oneshot(common::wait_request_agent(
            &shared,
            &h.tenant,
            Some("scout"),
            "when is the launch?",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(common::body_json(resp).await["text"], "from-scout");

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
}

#[tokio::test]
async fn run_start_event_records_the_agent() {
    let Some(h) = common::harness().await else {
        return;
    };
    let f = fixture(&h);
    let session = common::uid("t");
    let resp = f
        .app
        .oneshot(common::wait_request_agent(
            &session,
            &h.tenant,
            Some("coral"),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let events = h.store().read(&h.tenant, &session).await.unwrap();
    let agent = events.iter().find_map(|e| match &e.event {
        runic_substrate::SessionEvent::RunStart { agent, .. } => Some(agent.clone()),
        _ => None,
    });
    assert_eq!(agent.flatten().as_deref(), Some("coral"));
}

#[tokio::test]
async fn serve_config_builder_defaults_the_optional_infra() {
    let Some(h) = common::harness().await else {
        return;
    };
    let coral = EchoProvider::new("hi");
    let config = h.config().agent("coral", common::agent(coral));
    assert!(config.transcriber.is_none());
    assert!(config.identity.is_none());

    let app = router(config);
    let session = common::uid("t");
    let resp = app
        .oneshot(common::wait_request_agent(
            &session,
            &h.tenant,
            Some("coral"),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(common::body_json(resp).await["text"], "hi");
}

#[tokio::test]
async fn overview_groups_tools_skills_and_subagents_by_ability() {
    let Some(h) = common::harness().await else {
        return;
    };
    let rich = rich_agent(EchoProvider::new("unused")).await;
    let app = router(h.config().agent("rich", rich));
    let resp = app.oneshot(get_agent("rich", &h.tenant)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;

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
async fn overview_of_a_bare_agent_reports_no_abilities() {
    let Some(h) = common::harness().await else {
        return;
    };
    let f = fixture(&h);
    let resp = f.app.oneshot(get_agent("coral", &h.tenant)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = common::body_json(resp).await;
    assert!(
        body["abilities"].as_array().unwrap().is_empty(),
        "a plain agent with no tools/hooks/skills attached carries no ability to report"
    );
}

#[tokio::test]
async fn overview_of_an_unknown_agent_is_404() {
    let Some(h) = common::harness().await else {
        return;
    };
    let f = fixture(&h);
    let resp = f.app.oneshot(get_agent("ghost", &h.tenant)).await.unwrap();
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
    let Some(h) = common::harness().await else {
        return;
    };
    let config = h
        .config()
        .def(Support {
            provider: EchoProvider::new("supported"),
        })
        .await
        .unwrap();
    let app = router(config);

    let session = common::uid("t");
    let resp = app
        .clone()
        .oneshot(common::wait_request_agent(
            &session,
            &h.tenant,
            Some("support"),
            "hi",
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the name came off the AgentDef, not a hand-typed registry key"
    );

    let listed = common::body_json(
        app.oneshot(common::get("/agents", &h.tenant))
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
