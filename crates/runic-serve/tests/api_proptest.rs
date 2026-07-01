use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use proptest::prelude::*;
use serde_json::{Value, json};
use tower::ServiceExt;

use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{AgentFactory, HumanHub, ServeConfig, router};
use runic_state::SessionEvent;
use runic_substrate::{ArtifactStore, MemoryArtifactStore, MemorySessionStore, SessionStore};
use runic_types::{ContentBlock, MessageContent, StopReason, TokenUsage};

struct PanicFactory;

#[async_trait]
impl AgentFactory for PanicFactory {
    async fn build(&self, _: &str, _: &str) -> Agent {
        panic!("agent path must not run in CRUD props");
    }
}

struct ScriptedProvider;

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "pong".into(),
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
            },
        })
    }
}

struct ScriptedFactory;

#[async_trait]
impl AgentFactory for ScriptedFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        Agent::builder(Arc::new(ScriptedProvider), tenant, session_id)
            .system_prompt("test")
            .build()
    }
}

fn crud_router() -> Router {
    router(ServeConfig {
        session_store: Arc::new(MemorySessionStore::new()),
        artifact_store: Arc::new(MemoryArtifactStore::new()),
        transcriber: None,
        agent_factory: Arc::new(PanicFactory),
        human_hub: Arc::new(HumanHub::new()),
    })
}

fn scripted_full() -> (Router, Arc<dyn SessionStore>, Arc<dyn ArtifactStore>) {
    let sessions: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifactStore::new());
    let app = router(ServeConfig {
        session_store: sessions.clone(),
        artifact_store: artifacts.clone(),
        transcriber: None,
        agent_factory: Arc::new(ScriptedFactory),
        human_hub: Arc::new(HumanHub::new()),
    });
    (app, sessions, artifacts)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn get(uri: &str, tenant: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("x-runic-tenant", tenant)
        .body(Body::empty())
        .unwrap()
}

fn post_json(uri: &str, tenant: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-runic-tenant", tenant)
        .body(Body::from(body))
        .unwrap()
}

fn upload(thread: &str, tenant: &str, bytes: &[u8]) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/threads/{thread}/artifacts"))
        .header("content-type", "application/octet-stream")
        .header("x-runic-tenant", tenant)
        .body(Body::from(bytes.to_vec()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 2_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn drain(resp: axum::response::Response) {
    let _ = axum::body::to_bytes(resp.into_body(), 10_000_000)
        .await
        .unwrap();
}

async fn create_thread(app: &Router, tenant: &str, thread: &str) -> StatusCode {
    app.clone()
        .oneshot(post_json(
            "/threads",
            tenant,
            json!({ "thread_id": thread }).to_string(),
        ))
        .await
        .unwrap()
        .status()
}

fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

fn has_inline_bytes(events: &[runic_substrate::StoredEvent]) -> bool {
    events.iter().any(|s| {
        matches!(&s.event, SessionEvent::Message { msg, .. }
            if matches!(&msg.content, MessageContent::Blocks(b)
                if b.iter().any(|c| matches!(c, ContentBlock::Image { .. } | ContentBlock::File { .. }))))
    })
}

async fn wait_for_run_end(store: &dyn SessionStore, tenant: &str, thread: &str) {
    for _ in 0..100 {
        let events = store.read(tenant, thread).await.unwrap();
        if events
            .iter()
            .any(|s| matches!(s.event, SessionEvent::RunEnd { .. }))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn name() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,16}"
}

fn block_strategy() -> impl Strategy<Value = Value> {
    prop_oneof![
        "[a-z ]{1,12}".prop_map(|t| json!({ "type": "text", "text": t })),
        Just(json!({ "type": "image", "media_type": "image/png", "data": "aGVsbG8=" })),
        Just(json!({ "type": "image", "media_type": "image/png", "data": "!!not-base64!!" })),
        "[a-z0-9]{1,8}".prop_map(|s| {
            json!({ "type": "artifact_ref", "id": format!("art-{s}"), "media_type": "image/png" })
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn tenant_thread_namespace_never_collides(t1 in name(), t2 in name(), id in name()) {
        prop_assume!(t1 != t2);
        let app = crud_router();
        rt().block_on(async {
            prop_assert_eq!(create_thread(&app, &t1, &id).await, StatusCode::CREATED);

            let mine = app.clone().oneshot(get(&format!("/threads/{id}"), &t1)).await.unwrap();
            prop_assert_eq!(mine.status(), StatusCode::OK);
            let got = body_json(mine).await;
            prop_assert_eq!(got["thread_id"].as_str().unwrap(), id.as_str());

            let foreign = app.clone().oneshot(get(&format!("/threads/{id}"), &t2)).await.unwrap();
            prop_assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
            Ok(())
        })?;
    }

    #[test]
    fn pagination_walks_exactly_all_results(n in 1usize..24, page in 1usize..8) {
        let app = crud_router();
        rt().block_on(async {
            let tenant = "walker";
            let mut created: Vec<String> = (0..n).map(|i| format!("th-{i:03}")).collect();
            for id in &created {
                prop_assert_eq!(create_thread(&app, tenant, id).await, StatusCode::CREATED);
            }

            let mut seen: Vec<String> = Vec::new();
            let mut query = format!("?limit={page}");
            loop {
                let resp = app.clone().oneshot(get(&format!("/threads{query}"), tenant)).await.unwrap();
                prop_assert_eq!(resp.status(), StatusCode::OK);
                let body = body_json(resp).await;
                for t in body["threads"].as_array().unwrap() {
                    seen.push(t["thread_id"].as_str().unwrap().to_string());
                }
                match body["next_cursor"].as_str() {
                    Some(cursor) => query = format!("?limit={page}&cursor={}", urlencode(cursor)),
                    None => break,
                }
            }

            seen.sort();
            created.sort();
            prop_assert_eq!(seen, created);
            Ok(())
        })?;
    }

    #[test]
    fn inline_bytes_never_reach_the_event_log(blocks in prop::collection::vec(block_strategy(), 1..4)) {
        let (app, sessions, _artifacts) = scripted_full();
        rt().block_on(async {
            prop_assert_eq!(create_thread(&app, "alice", "props").await, StatusCode::CREATED);

            let body = json!({ "content": blocks }).to_string();
            let resp = app
                .clone()
                .oneshot(post_json("/threads/props/runs/stream", "alice", body))
                .await
                .unwrap();
            let status = resp.status();
            prop_assert!(status == StatusCode::OK || status == StatusCode::BAD_REQUEST);

            if status == StatusCode::OK {
                drain(resp).await;
                wait_for_run_end(sessions.as_ref(), "alice", "props").await;
            }

            let events = sessions.read("alice", "props").await.unwrap();
            prop_assert!(!has_inline_bytes(&events));
            Ok(())
        })?;
    }

    #[test]
    fn artifact_refs_never_cross_ownership(owner in name(), attacker in name()) {
        prop_assume!(owner != attacker);
        let (app, sessions, _artifacts) = scripted_full();
        rt().block_on(async {
            prop_assert_eq!(create_thread(&app, &owner, "vault").await, StatusCode::CREATED);
            let up = app.clone().oneshot(upload("vault", &owner, b"secret")).await.unwrap();
            prop_assert_eq!(up.status(), StatusCode::CREATED);
            let id = body_json(up).await["id"].as_str().unwrap().to_string();

            let ref_body = json!({
                "content": [{ "type": "artifact_ref", "id": id, "media_type": "image/png" }]
            })
            .to_string();

            let stolen = app
                .clone()
                .oneshot(post_json("/threads/vault/runs/stream", &attacker, ref_body.clone()))
                .await
                .unwrap();
            prop_assert_eq!(stolen.status(), StatusCode::BAD_REQUEST);

            let owned = app
                .clone()
                .oneshot(post_json("/threads/vault/runs/stream", &owner, ref_body))
                .await
                .unwrap();
            prop_assert_eq!(owned.status(), StatusCode::OK);
            drain(owned).await;
            wait_for_run_end(sessions.as_ref(), &owner, "vault").await;
            prop_assert!(!has_inline_bytes(&sessions.read(&owner, "vault").await.unwrap()));
            Ok(())
        })?;
    }
}
