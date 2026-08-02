mod common;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::StatusCode;
use proptest::prelude::*;
use serde_json::{Value, json};
use tower::ServiceExt;

use runic::substrate::{ArtifactSource, SessionEvent};
use runic::types::{ContentBlock, MessageContent, StopReason, TokenUsage};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::ServeError;
use runic_serve::app::AppState;
use runic_serve::hosts::AgentRegistry;
use runic_serve::routes::runs::input::{RunMessageRequest, input_from_message};

struct PanicProvider;

#[async_trait]
impl Provider for PanicProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
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
                ..Default::default()
            },
        })
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

fn has_inline_bytes(events: &[runic::substrate::StoredEvent]) -> bool {
    events.iter().any(|s| {
        matches!(&s.event, SessionEvent::Message { msg, .. }
            if matches!(&msg.content, MessageContent::Blocks(b)
                if b.iter().any(|c| matches!(c, ContentBlock::Image { .. } | ContentBlock::File { .. }))))
    })
}

fn name() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,16}"
}

fn valid_block_strategy() -> impl Strategy<Value = Value> {
    prop_oneof![
        "[a-z ]{1,12}".prop_map(|t| json!({ "type": "text", "text": t })),
        Just(json!({ "type": "image", "media_type": "image/png", "data": "aGVsbG8=" })),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn tenant_session_namespace_never_collides(t1 in name(), t2 in name(), id in name()) {
        prop_assume!(t1 != t2);
        rt().block_on(async {
            let Some(h) = common::harness().await else { return Ok(()) };
            let app = h.single_router(common::agent(Arc::new(PanicProvider)));
            common::create_session(&app, &t1, &id).await;

            let mine = app.clone().oneshot(common::get(&format!("/sessions/{id}"), &t1)).await.unwrap();
            prop_assert_eq!(mine.status(), StatusCode::OK);
            let got = common::body_json(mine).await;
            prop_assert_eq!(got["session_id"].as_str().unwrap(), id.as_str());

            let foreign = app.clone().oneshot(common::get(&format!("/sessions/{id}"), &t2)).await.unwrap();
            prop_assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
            Ok(())
        })?;
    }

    #[test]
    fn pagination_walks_exactly_all_results(n in 1usize..24, page in 1usize..8) {
        rt().block_on(async {
            let Some(h) = common::harness().await else { return Ok(()) };
            let app = h.single_router(common::agent(Arc::new(PanicProvider)));
            let tenant = h.tenant.clone();
            let mut created: Vec<String> = (0..n).map(|i| format!("th-{i:03}")).collect();
            for id in &created {
                common::create_session(&app, &tenant, id).await;
            }

            let mut seen: Vec<String> = Vec::new();
            let mut query = format!("?limit={page}");
            loop {
                let resp = app.clone().oneshot(common::get(&format!("/sessions{query}"), &tenant)).await.unwrap();
                prop_assert_eq!(resp.status(), StatusCode::OK);
                let body = common::body_json(resp).await;
                for t in body["sessions"].as_array().unwrap() {
                    seen.push(t["session_id"].as_str().unwrap().to_string());
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
    fn any_mix_of_valid_content_blocks_completes_the_run(blocks in prop::collection::vec(valid_block_strategy(), 1..4)) {
        rt().block_on(async {
            let Some(h) = common::harness().await else { return Ok(()) };
            let store = h.store();
            let tenant = h.tenant.clone();
            let app = h.single_router(common::agent(Arc::new(ScriptedProvider)));
            common::create_session(&app, &tenant, "props").await;

            let body = json!({ "content": blocks }).to_string();
            let resp = app
                .clone()
                .oneshot(common::post_json("/sessions/props/runs/wait", &tenant, body))
                .await
                .unwrap();
            prop_assert_eq!(resp.status(), StatusCode::OK);
            let body = common::body_json(resp).await;
            prop_assert_eq!(body["text"].as_str().unwrap(), "pong");

            let events = store.read(&tenant, "props").await.unwrap();
            let ended = events
                .iter()
                .any(|stored| matches!(&stored.event, SessionEvent::RunEnd { .. }));
            prop_assert!(ended);
            Ok(())
        })?;
    }

    #[test]
    fn artifact_refs_never_cross_ownership(owner in name(), attacker in name()) {
        prop_assume!(owner != attacker);
        rt().block_on(async {
            let Some(h) = common::harness().await else { return Ok(()) };
            let app = h.single_router(common::agent(Arc::new(ScriptedProvider)));
            common::create_session(&app, &owner, "vault").await;
            let art = h
                .artifacts()
                .put(&owner, "vault", "text/plain", ArtifactSource::UserUpload, b"secret")
                .await
                .unwrap();

            let state = AppState {
                sessions: h.sessions.clone(),
                blobs: h.blobs.clone(),
                runs: h.runs(),
                pool: h.pool.clone(),
                events: runic_serve::stream::LocalEvents::new(),
                transcriber: None,
                agents: Arc::new(AgentRegistry::new(HashMap::from([(
                    "main".to_string(),
                    common::agent(Arc::new(ScriptedProvider)).into(),
                )]))),
                completions: runic_serve::completion::Completions::new(),
                hooks: Arc::new(runic_serve::hook::HookRegistry::default()),
                schedules: runic_serve::store::Schedules::new(h.pool.clone()),
                routines: Arc::new(runic_serve::routines::RoutineRegistry::default()),
            };
            let ref_body: RunMessageRequest = serde_json::from_value(json!({
                "content": [{ "type": "artifact_ref", "id": art.id, "media_type": "image/png" }]
            }))
            .unwrap();
            let stolen = input_from_message(&state, &attacker, "vault", ref_body.into_message().unwrap()).await;
            prop_assert!(matches!(stolen, Err(ServeError::BadRequest(_))));

            let resp = app
                .clone()
                .oneshot(common::post_json(
                    "/sessions/vault/runs/wait",
                    &owner,
                    json!({ "content": [{ "type": "artifact_ref", "id": art.id, "media_type": "image/png" }] })
                        .to_string(),
                ))
                .await
                .unwrap();
            prop_assert!(resp.status() == StatusCode::OK || resp.status() == StatusCode::INTERNAL_SERVER_ERROR);
            let store = h.store();
            prop_assert!(!has_inline_bytes(&store.read(&owner, "vault").await.unwrap()));
            Ok(())
        })?;
    }
}
