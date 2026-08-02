mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use runic::substrate::SessionStore;
use runic::types::{ContentBlock, StopReason, TokenUsage};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{RunSpec, RunStatus};

use common::Harness;

struct PanicProvider;

#[async_trait]
impl Provider for PanicProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        panic!("agent path must not run in CRUD tests");
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

fn crud_router(h: &Harness) -> Router {
    h.single_router(common::agent(Arc::new(PanicProvider)))
}

fn scripted_router(h: &Harness) -> Router {
    h.single_router(common::agent(Arc::new(ScriptedProvider)))
}

async fn create_labeled(app: &Router, tenant: &str, id: &str, label: Value) -> Value {
    let body = json!({ "session_id": id, "label": label }).to_string();
    let resp = app
        .clone()
        .oneshot(common::post_json("/sessions", tenant, body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    common::body_json(resp).await
}

async fn wait_for_stored_events(
    store: &dyn SessionStore,
    tenant: &str,
    session_id: &str,
    min_events: usize,
) {
    for _ in 0..50 {
        if store.read(tenant, session_id).await.unwrap().len() >= min_events {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("stored event count did not reach {min_events}");
}

async fn list(app: &Router, tenant: &str, query: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(common::get(&format!("/sessions{query}"), tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    common::body_json(resp).await
}

fn page_ids(page: &Value) -> Vec<String> {
    page["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["session_id"].as_str().unwrap().to_string())
        .collect()
}

fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

#[tokio::test]
async fn list_limit_clamps_to_upper_bound_of_200() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    for i in 0..205 {
        common::create_session(&app, &h.tenant, &format!("t{i:03}")).await;
    }
    let page = list(&app, &h.tenant, "?limit=1000").await;
    assert_eq!(page["sessions"].as_array().unwrap().len(), 200);
    assert!(page["next_cursor"].is_string());
}

#[tokio::test]
async fn list_limit_clamps_to_lower_bound_of_1() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_session(&app, &h.tenant, "a").await;
    common::create_session(&app, &h.tenant, "b").await;
    let page = list(&app, &h.tenant, "?limit=0").await;
    assert_eq!(page["sessions"].as_array().unwrap().len(), 1);
    assert!(page["next_cursor"].is_string());
}

#[tokio::test]
async fn next_cursor_absent_when_page_exhausts_results() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    common::create_session(&app, &h.tenant, "only-a").await;
    common::create_session(&app, &h.tenant, "only-b").await;
    let page = list(&app, &h.tenant, "?limit=50").await;
    assert_eq!(page["sessions"].as_array().unwrap().len(), 2);
    assert!(page["next_cursor"].is_null());
}

#[tokio::test]
async fn walking_cursor_covers_every_session_once() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let mut created: Vec<String> = (0..25).map(|i| format!("t{i:02}")).collect();
    for id in &created {
        common::create_session(&app, &h.tenant, id).await;
    }

    let mut seen: Vec<String> = Vec::new();
    let mut query = "?limit=7".to_string();
    loop {
        let page = list(&app, &h.tenant, &query).await;
        seen.extend(page_ids(&page));
        match page["next_cursor"].as_str() {
            Some(cursor) => query = format!("?limit=7&cursor={}", urlencode(cursor)),
            None => break,
        }
    }

    seen.sort();
    created.sort();
    assert_eq!(seen, created);
}

#[tokio::test]
async fn listing_is_newest_active_first() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    for id in ["oldest", "middle", "newest"] {
        common::create_session(&app, &h.tenant, id).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let page = list(&app, &h.tenant, "?limit=50").await;
    assert_eq!(page_ids(&page), vec!["newest", "middle", "oldest"]);
}

#[tokio::test]
async fn cursor_is_tenant_scoped_and_cannot_leak_foreign_sessions() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let alice = format!("{}-alice", h.tenant);
    let bob = format!("{}-bob", h.tenant);
    for i in 0..5 {
        common::create_session(&app, &alice, &format!("alice-{i}")).await;
        common::create_session(&app, &bob, &format!("bob-{i}")).await;
    }

    let alice_page = list(&app, &alice, "?limit=2").await;
    let cursor = alice_page["next_cursor"].as_str().unwrap();

    let bob_page = list(
        &app,
        &bob,
        &format!("?limit=50&cursor={}", urlencode(cursor)),
    )
    .await;
    let bob_ids = page_ids(&bob_page);
    assert!(
        bob_ids.iter().all(|id| id.starts_with("bob-")),
        "{bob_ids:?}"
    );
}

#[tokio::test]
async fn create_trims_label_whitespace() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let body = create_labeled(&app, &h.tenant, "trim", json!("  spaced  ")).await;
    assert_eq!(body["label"], "spaced");
}

#[tokio::test]
async fn create_whitespace_label_becomes_null() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let body = create_labeled(&app, &h.tenant, "blank", json!("   ")).await;
    assert!(body["label"].is_null());
}

#[tokio::test]
async fn recreate_without_label_preserves_existing() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    create_labeled(&app, &h.tenant, "keepme", json!("original")).await;
    let resp = app
        .clone()
        .oneshot(common::post_json(
            "/sessions",
            &h.tenant,
            json!({ "session_id": "keepme" }).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(common::body_json(resp).await["label"], "original");
}

#[tokio::test]
async fn recreate_with_label_updates_it() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    create_labeled(&app, &h.tenant, "reup", json!("first")).await;
    let body = create_labeled(&app, &h.tenant, "reup", json!("second")).await;
    assert_eq!(body["label"], "second");
}

#[tokio::test]
async fn patch_sets_clears_and_leaves_label_unchanged() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    create_labeled(&app, &h.tenant, "patchme", json!("start")).await;

    let resp = app
        .clone()
        .oneshot(common::patch_json(
            "/sessions/patchme",
            &h.tenant,
            r#"{"label":"renamed"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(common::body_json(resp).await["label"], "renamed");

    let resp = app
        .clone()
        .oneshot(common::patch_json("/sessions/patchme", &h.tenant, "{}"))
        .await
        .unwrap();
    assert_eq!(common::body_json(resp).await["label"], "renamed");

    let resp = app
        .clone()
        .oneshot(common::patch_json(
            "/sessions/patchme",
            &h.tenant,
            r#"{"label":null}"#,
        ))
        .await
        .unwrap();
    assert!(common::body_json(resp).await["label"].is_null());
}

#[tokio::test]
async fn patch_unknown_session_is_404() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = crud_router(&h);
    let resp = app
        .oneshot(common::patch_json(
            "/sessions/ghost",
            &h.tenant,
            r#"{"label":"x"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

async fn seed_run_events(h: &Harness) -> Router {
    let app = scripted_router(h);
    let resp = app
        .clone()
        .oneshot(common::wait_request("evsession", &h.tenant, "hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    wait_for_stored_events(h.store().as_ref(), &h.tenant, "evsession", 4).await;
    app
}

async fn events_page(app: &Router, tenant: &str, query: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(common::get(
            &format!("/sessions/evsession/events{query}"),
            tenant,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    common::body_json(resp).await
}

#[tokio::test]
async fn events_pagination_walks_all_seqs_without_gaps() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = seed_run_events(&h).await;
    let total = h.store().read(&h.tenant, "evsession").await.unwrap().len();

    let mut seqs: Vec<u64> = Vec::new();
    let mut after = 0u64;
    loop {
        let page = events_page(&app, &h.tenant, &format!("?after_seq={after}&limit=2")).await;
        for e in page["events"].as_array().unwrap() {
            seqs.push(e["seq"].as_u64().unwrap());
        }
        if page["has_more"].as_bool().unwrap() {
            after = page["next_after_seq"].as_u64().unwrap();
        } else {
            break;
        }
    }

    assert_eq!(seqs.len(), total);
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
}

#[tokio::test]
async fn events_has_more_and_next_after_seq_are_consistent() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = seed_run_events(&h).await;
    let total = h.store().read(&h.tenant, "evsession").await.unwrap().len();

    let first = events_page(&app, &h.tenant, "?limit=2").await;
    assert_eq!(first["events"].as_array().unwrap().len(), 2);
    assert_eq!(first["has_more"], true);
    assert!(first["next_after_seq"].is_u64());

    let all = events_page(&app, &h.tenant, &format!("?limit={}", total + 10)).await;
    assert_eq!(all["events"].as_array().unwrap().len(), total);
    assert_eq!(all["has_more"], false);
}

#[tokio::test]
async fn events_limit_clamps_to_lower_bound_of_1() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = seed_run_events(&h).await;
    let page = events_page(&app, &h.tenant, "?limit=0").await;
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(page["has_more"], true);
}

#[tokio::test]
async fn events_for_wrong_tenant_is_404_not_foreign_events() {
    let Some(h) = common::harness().await else {
        return;
    };
    let app = seed_run_events(&h).await;
    let resp = app
        .oneshot(common::get("/sessions/evsession/events", "mallory"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn children_are_listed_separately_and_deleted_with_the_parent() {
    let Some(h) = common::harness().await else {
        return;
    };
    let store = h.store();
    let app = scripted_router(&h);
    let tenant = h.tenant.clone();

    common::create_session(&app, &tenant, "parent-1").await;
    for (child, grandchild) in [("chd-a", None), ("chd-b", Some("chd-b-1"))] {
        store
            .create_child_session(&tenant, child, "parent-1", "scout")
            .await
            .unwrap();
        store
            .append(
                &tenant,
                child,
                &runic::substrate::SessionEvent::RunStart {
                    run_id: format!("r-{child}"),
                    agent: Some("scout".into()),
                    audit: None,
                    at: chrono::Utc::now(),
                },
            )
            .await
            .unwrap();
        if let Some(grandchild) = grandchild {
            store
                .create_child_session(&tenant, grandchild, child, "scribe")
                .await
                .unwrap();
        }
    }

    let resp = app
        .clone()
        .oneshot(common::get("/sessions", &tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = common::body_json(resp).await;
    let ids: Vec<&str> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["session_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"parent-1"));
    assert!(
        !ids.iter().any(|id| id.starts_with("chd-")),
        "session list must exclude child sessions: {ids:?}"
    );

    let resp = app
        .clone()
        .oneshot(common::get("/sessions/parent-1/children", &tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let children = common::body_json(resp).await;
    let rows = children["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row["parent_session"], "parent-1");
        assert_eq!(row["agent"], "scout");
    }

    let resp = app
        .clone()
        .oneshot(common::get("/sessions/parent-1/children", "mallory"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "cross-tenant child listing must be rejected"
    );

    let resp = app
        .clone()
        .oneshot(common::delete("/sessions/parent-1", &tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    for session in ["parent-1", "chd-a", "chd-b", "chd-b-1"] {
        assert!(
            store
                .session_meta(&tenant, session)
                .await
                .unwrap()
                .is_none(),
            "{session} must be gone after recursive delete"
        );
    }
}

#[tokio::test]
async fn delete_is_refused_while_a_run_is_active_on_the_session() {
    let Some(h) = common::harness().await else {
        return;
    };
    let store = h.store();
    let app = scripted_router(&h);
    let tenant = h.tenant.clone();
    common::create_session(&app, &tenant, "busy").await;

    let runs = h.runs();
    let run_id = common::uid("run");
    runs.enqueue(&RunSpec::new(&tenant, &run_id, "main").session("busy"))
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(common::delete("/sessions/busy", &tenant))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "delete must be refused while a run is active on the session"
    );
    assert!(
        store.session_meta(&tenant, "busy").await.unwrap().is_some(),
        "the refused delete must not touch the session"
    );

    runs.finish(&run_id, RunStatus::Successful, None, None)
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(common::delete("/sessions/busy", &tenant))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(store.session_meta(&tenant, "busy").await.unwrap().is_none());
}
