use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use redis::AsyncCommands;
use runic_serve::{EventBroker, RedisBroker};
use runic_state::SessionEvent;
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
