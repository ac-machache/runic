use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use redis::AsyncCommands;
use runic_serve::{EventBroker, QueueNudge, RedisBroker};
use runic_state::SessionEvent;
use runic_substrate::SessionStore;
use runic_types::Message;

async fn broker() -> Option<std::sync::Arc<RedisBroker>> {
    let Ok(url) = std::env::var("RUNIC_TEST_REDIS_URL") else {
        static NOTED: AtomicBool = AtomicBool::new(false);
        if !NOTED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "\n⚠  RUNIC_TEST_REDIS_URL not set — redis_broker tests SKIPPED (NOT verified).\n"
            );
        }
        return None;
    };
    Some(
        RedisBroker::connect(&url)
            .await
            .expect("RUNIC_TEST_REDIS_URL is set but unreachable"),
    )
}

fn redis_url() -> Option<String> {
    std::env::var("RUNIC_TEST_REDIS_URL").ok()
}

fn event(text: &str) -> SessionEvent {
    SessionEvent::Message {
        run_id: "r-redis".into(),
        msg: Message::user(text),
        at: Utc::now(),
    }
}

fn unique_prefix() -> String {
    format!("runic:test:{}", uuid::Uuid::new_v4().simple())
}

#[tokio::test]
async fn publish_reaches_subscribers_and_channels_are_isolated() {
    let Some(broker) = broker().await else {
        return;
    };
    let thread = format!("t-{}", uuid::Uuid::new_v4().simple());
    let mut rx = broker.subscribe("alice", &thread).await.expect("subscribe");
    let mut other = broker
        .subscribe("alice", &format!("{thread}-other"))
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(100)).await;

    broker.publish("alice", &thread, &event("over redis")).await;

    let received = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("event within 5s")
        .expect("channel open");
    match received {
        SessionEvent::Message { msg, run_id, .. } => {
            assert_eq!(run_id, "r-redis");
            assert!(msg.content.text_content().contains("over redis"));
        }
        other => panic!("unexpected event: {other:?}"),
    }

    assert!(
        tokio::time::timeout(Duration::from_millis(300), other.recv())
            .await
            .is_err(),
        "channel isolation"
    );
}

#[tokio::test]
async fn tenant_and_thread_are_both_part_of_channel_identity() {
    let Some(broker) = broker().await else {
        return;
    };
    let broker = broker.with_prefix(unique_prefix());
    let thread = format!("t-{}", uuid::Uuid::new_v4().simple());
    let mut same = broker.subscribe("alice", &thread).await.expect("subscribe");
    let mut other_tenant = broker.subscribe("bob", &thread).await.expect("subscribe");
    let mut other_thread = broker
        .subscribe("alice", &format!("{thread}-other"))
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(100)).await;

    broker.publish("alice", &thread, &event("scoped")).await;

    let received = tokio::time::timeout(Duration::from_secs(5), same.recv())
        .await
        .expect("event within 5s")
        .expect("channel open");
    match received {
        SessionEvent::Message { msg, .. } => {
            assert!(msg.content.text_content().contains("scoped"));
        }
        other => panic!("unexpected event: {other:?}"),
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(300), other_tenant.recv())
            .await
            .is_err(),
        "tenant isolation"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), other_thread.recv())
            .await
            .is_err(),
        "thread isolation"
    );
}

#[tokio::test]
async fn colon_separated_channel_names_do_not_collide() {
    let Some(broker) = broker().await else {
        return;
    };
    let broker = broker.with_prefix(unique_prefix());
    let mut left = broker
        .subscribe("tenant:one", "thread")
        .await
        .expect("subscribe");
    let mut right = broker
        .subscribe("tenant", "one:thread")
        .await
        .expect("subscribe");
    tokio::time::sleep(Duration::from_millis(100)).await;

    broker
        .publish("tenant:one", "thread", &event("left-only"))
        .await;

    let received = tokio::time::timeout(Duration::from_secs(5), left.recv())
        .await
        .expect("event within 5s")
        .expect("channel open");
    match received {
        SessionEvent::Message { msg, .. } => {
            assert!(msg.content.text_content().contains("left-only"));
        }
        other => panic!("unexpected event: {other:?}"),
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(300), right.recv())
            .await
            .is_err(),
        "tenant/thread values containing ':' must not share a Redis channel"
    );
}

#[tokio::test]
async fn a_nudge_wakes_a_blocked_waiter_across_connections() {
    let Some(sender) = broker().await else {
        return;
    };
    let key = format!("{}:queue", unique_prefix());
    let sender = sender.with_nudge_key(key.clone());
    let waiter = broker()
        .await
        .expect("second connection")
        .with_nudge_key(key);

    let woke = tokio::spawn(async move {
        let start = std::time::Instant::now();
        waiter.wait(Duration::from_secs(10)).await;
        start.elapsed()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    sender.nudge().await;

    let elapsed = tokio::time::timeout(Duration::from_secs(5), woke)
        .await
        .expect("waiter woke within 5s")
        .expect("waiter task");
    assert!(
        elapsed < Duration::from_secs(5),
        "waiter should wake on the nudge, not the 10s timeout"
    );
}

#[tokio::test]
async fn a_pending_nudge_token_wakes_the_next_waiter_immediately() {
    let Some(broker) = broker().await else {
        return;
    };
    let broker = broker.with_nudge_key(format!("{}:queue", unique_prefix()));

    broker.nudge().await;

    let start = std::time::Instant::now();
    broker.wait(Duration::from_secs(10)).await;
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "a queued token must satisfy the next wait without blocking"
    );
}

struct EchoProvider;

#[async_trait::async_trait]
impl runic_provider::Provider for EchoProvider {
    async fn complete(
        &self,
        _req: runic_provider::CompletionRequest,
    ) -> Result<runic_provider::CompletionResponse, runic_provider::ProviderError> {
        Ok(runic_provider::CompletionResponse {
            content: vec![runic_types::ContentBlock::Text {
                text: "ok".into(),
                provider_metadata: None,
            }],
            stop_reason: runic_types::StopReason::EndTurn,
            tool_calls: vec![],
            usage: runic_types::TokenUsage::default(),
        })
    }
}

struct EchoFactory;

#[async_trait::async_trait]
impl runic_serve::AgentFactory for EchoFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> anyhow::Result<runic_agent::Agent> {
        Ok(
            runic_agent::Agent::builder(std::sync::Arc::new(EchoProvider), tenant, session_id)
                .system_prompt("test")
                .build(),
        )
    }
}

fn instance_config(
    store: std::sync::Arc<dyn runic_substrate::SessionStore>,
    nudge: std::sync::Arc<RedisBroker>,
) -> runic_serve::ServeConfig {
    runic_serve::ServeConfig {
        session_store: store,
        artifact_store: std::sync::Arc::new(runic_substrate::MemoryArtifactStore::new()),
        transcriber: None,
        agents: runic_serve::single_agent("main", std::sync::Arc::new(EchoFactory)),
        limits: Default::default(),
        workers: Some(runic_serve::WorkerConfig {
            max_concurrent_runs: 2,
            poll_every: Duration::from_secs(120),
        }),
        broker: None,
        nudge: Some(nudge),
        identity: None,
    }
}

#[tokio::test]
async fn an_enqueue_on_one_instance_wakes_a_worker_on_another_via_redis() {
    use tower::ServiceExt;

    let Some(nudge_a) = broker().await else {
        return;
    };
    let key = format!("{}:queue", unique_prefix());
    let nudge_a = nudge_a.with_nudge_key(key.clone());
    let nudge_b = broker()
        .await
        .expect("second connection")
        .with_nudge_key(key);

    let store: std::sync::Arc<runic_substrate::MemorySessionStore> =
        std::sync::Arc::new(runic_substrate::MemorySessionStore::new());
    let api_only = runic_serve::bare_router(instance_config(store.clone(), nudge_a));
    let _executor = runic_serve::router(instance_config(store.clone(), nudge_b));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = api_only
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/threads/t1/runs")
                .header("content-type", "application/json")
                .header("x-runic-tenant", "alice")
                .body(axum::body::Body::from(
                    serde_json::json!({ "message": "go" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let run_id = body["run_id"].as_str().unwrap().to_string();

    for _ in 0..300 {
        let rec = store.get_run("alice", &run_id).await.unwrap().unwrap();
        if rec.status.is_terminal() {
            assert_eq!(rec.status, runic_substrate::RunStatus::Success);
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the run never executed — the enqueuing instance has no workers, \
         so only a Redis-woken worker on the other instance could run it"
    );
}

#[tokio::test]
async fn invalid_payload_is_ignored_without_closing_the_subscription() {
    let Some(url) = redis_url() else {
        let _ = broker().await;
        return;
    };
    let Some(broker) = broker().await else {
        return;
    };
    let prefix = unique_prefix();
    let broker = broker.with_prefix(prefix.clone());
    let tenant = "alice";
    let thread = format!("t-{}", uuid::Uuid::new_v4().simple());
    let mut rx = broker.subscribe(tenant, &thread).await.expect("subscribe");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = redis::Client::open(url).expect("redis url");
    let mut conn = client
        .get_connection_manager()
        .await
        .expect("redis connection");
    let channel = format!("{prefix}:{tenant}:{thread}");
    conn.publish::<_, _, ()>(&channel, "{not-json")
        .await
        .expect("publish invalid payload");
    broker
        .publish(tenant, &thread, &event("after invalid"))
        .await;

    let received = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("valid event within 5s")
        .expect("channel open");
    match received {
        SessionEvent::Message { msg, .. } => {
            assert!(msg.content.text_content().contains("after invalid"));
        }
        other => panic!("unexpected event: {other:?}"),
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "the invalid payload must not be surfaced as an event"
    );
}
