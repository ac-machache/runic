use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use runic_e2e_harness::dummy_agents;
use runic_serve::{EventBroker, RedisBroker, ServeConfig, WorkerConfig, router};
use runic_substrate::{
    ArtifactStore, MemoryArtifactStore, MemorySessionStore, SessionEvent, SessionStore,
};
use serde_json::json;
use tower::ServiceExt;

fn unique(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{tag}{nanos}")
}

async fn broker(prefix: &str) -> Option<Arc<RedisBroker>> {
    let url = std::env::var("REDIS_URL").ok()?;
    let b = RedisBroker::connect(&url)
        .await
        .expect("REDIS_URL unreachable");
    Some(b.with_prefix(format!("runic:test:{prefix}")))
}

fn post(uri: &str, tenant: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-runic-tenant", tenant)
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn redis_broker_round_trips_an_event() {
    rt().block_on(async {
        let Some(b) = broker(&unique("rt")).await else {
            eprintln!("skipped: REDIS_URL unset");
            return;
        };
        let (t, th) = ("tenantA", "threadA");
        let mut rx = b.subscribe(t, th).await.expect("subscribe failed");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let evt = SessionEvent::RunStart {
            run_id: "r1".into(),
            agent: None,
            audit: None,
            at: chrono::Utc::now(),
        };
        b.publish(t, th, &evt).await;

        let got = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("no event within timeout")
            .expect("broker channel closed");
        match got {
            SessionEvent::RunStart { run_id, .. } => assert_eq!(run_id, "r1"),
            other => panic!("unexpected event over redis: {other:?}"),
        }
    });
}

#[test]
fn cross_tenant_channels_do_not_leak() {
    rt().block_on(async {
        let Some(b) = broker(&unique("iso")).await else {
            return;
        };
        let mut rx = b
            .subscribe("tenantX", "th")
            .await
            .expect("subscribe failed");
        tokio::time::sleep(Duration::from_millis(200)).await;

        b.publish(
            "tenantY",
            "th",
            &SessionEvent::RunStart {
                run_id: "foreign".into(),
                agent: None,
                audit: None,
                at: chrono::Utc::now(),
            },
        )
        .await;
        b.publish(
            "tenantX",
            "th",
            &SessionEvent::RunStart {
                run_id: "mine".into(),
                agent: None,
                audit: None,
                at: chrono::Utc::now(),
            },
        )
        .await;

        let got = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("no event within timeout")
            .expect("broker channel closed");
        match got {
            SessionEvent::RunStart { run_id, .. } => {
                assert_eq!(
                    run_id, "mine",
                    "received an event from another tenant's channel"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    });
}

#[test]
fn run_events_fan_out_across_instances_via_redis() {
    rt().block_on(async {
        let Some(b) = broker(&unique("fan")).await else {
            return;
        };
        let store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        let artifacts: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifactStore::new());
        let mk = || {
            router(
                ServeConfig::new(store.clone(), artifacts.clone(), dummy_agents(false))
                    .workers(WorkerConfig {
                        max_concurrent_runs: 4,
                        poll_every: Duration::from_millis(10),
                    })
                    .broker(b.clone())
                    .nudge(b.clone()),
            )
        };
        let app_a = mk();
        let _app_b = mk();

        let (t, th) = ("tf".to_string(), "thf".to_string());
        let mut rx = b.subscribe(&t, &th).await.expect("subscribe failed");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let resp = app_a
            .clone()
            .oneshot(post(
                &format!("/threads/{th}/runs"),
                &t,
                json!({ "message": "say:hello" }),
            ))
            .await
            .unwrap();
        let run_id = body_json(resp).await["run_id"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(!run_id.is_empty(), "no run_id");

        let mut saw_start = false;
        let mut saw_end = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while tokio::time::Instant::now() < deadline && !(saw_start && saw_end) {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Some(SessionEvent::RunStart { run_id: r, .. })) if r == run_id => {
                    saw_start = true
                }
                Ok(Some(SessionEvent::RunEnd { run_id: r, .. })) if r == run_id => saw_end = true,
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {}
            }
        }
        assert!(saw_end, "never received RunEnd via redis fan-out");
        assert!(saw_start, "never received RunStart via redis fan-out");
    });
}
