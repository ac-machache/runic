#![cfg(feature = "redis")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use runic_serve::WireEvent;
use runic_serve::{RedisEvents, RunEvents};

async fn sink() -> Option<Arc<RedisEvents>> {
    let Ok(url) = std::env::var("RUNIC_TEST_REDIS_URL") else {
        static NOTED: AtomicBool = AtomicBool::new(false);
        if !NOTED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "\n⚠  RUNIC_TEST_REDIS_URL not set — runic-serve redis tests SKIPPED (NOT verified). \
                 Point it at a scratch Redis to verify (see scripts/test-redis.sh).\n"
            );
        }
        return None;
    };
    Some(
        RedisEvents::connect(&url)
            .await
            .expect("RUNIC_TEST_REDIS_URL is set but unreachable"),
    )
}

fn run_id() -> String {
    format!("r-{}", uuid::Uuid::new_v4().simple())
}

fn delta(text: &str) -> WireEvent {
    WireEvent::AssistantTextDelta {
        text: text.to_string(),
    }
}

fn spoken(event: &WireEvent) -> String {
    match event {
        WireEvent::AssistantTextDelta { text } => text.clone(),
        other => panic!("expected a text delta, got {other:?}"),
    }
}

async fn within<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("redis did not answer in time")
}

#[tokio::test]
async fn one_instance_reads_what_another_instance_published() {
    let (Some(writer), Some(reader)) = (sink().await, sink().await) else {
        return;
    };
    let run = run_id();

    writer.publish(&run, delta("hello "));
    writer.publish(&run, delta("world"));

    let replay = within(reader.since(&run, 0)).await;
    let seen: Vec<String> = replay.events.iter().map(|(_, e)| spoken(e)).collect();
    assert!(
        seen.starts_with(&["hello ".to_string()]),
        "the reader saw {seen:?}"
    );
    assert!(!replay.gap);
    assert!(!replay.closed);
}

#[tokio::test]
async fn sequence_numbers_start_at_one_and_do_not_repeat_on_resume() {
    let (Some(writer), Some(reader)) = (sink().await, sink().await) else {
        return;
    };
    let run = run_id();

    for index in 0..5 {
        writer.publish(&run, delta(&index.to_string()));
    }

    let mut cursor = 0;
    let mut collected: Vec<(u64, String)> = Vec::new();
    while collected.len() < 5 {
        let replay = within(reader.since(&run, cursor)).await;
        for (seq, event) in replay.events {
            cursor = seq;
            collected.push((seq, spoken(&event)));
        }
    }

    let seqs: Vec<u64> = collected.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5], "sequence is dense and 1-based");
    let texts: Vec<&str> = collected.iter().map(|(_, text)| text.as_str()).collect();
    assert_eq!(texts, vec!["0", "1", "2", "3", "4"], "order is preserved");
}

#[tokio::test]
async fn a_reader_waiting_on_an_idle_run_wakes_when_an_event_arrives() {
    let (Some(writer), Some(reader)) = (sink().await, sink().await) else {
        return;
    };
    let run = run_id();

    let waiting = tokio::spawn({
        let reader = Arc::clone(&reader);
        let run = run.clone();
        async move { reader.since(&run, 0).await }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !waiting.is_finished(),
        "nothing published yet, so nothing to return"
    );

    writer.publish(&run, delta("late"));
    let replay = within(waiting).await.expect("the waiter joined");
    assert_eq!(replay.events.len(), 1);
    assert_eq!(spoken(&replay.events[0].1), "late");
}

#[tokio::test]
async fn finishing_a_run_closes_it_for_every_instance() {
    let (Some(writer), Some(reader)) = (sink().await, sink().await) else {
        return;
    };
    let run = run_id();

    writer.publish(&run, delta("working"));
    within(reader.since(&run, 0)).await;

    writer.finish(&run);

    let replay = within(reader.since(&run, 1)).await;
    assert!(replay.closed, "the reader must learn the run ended");
    assert!(
        replay.events.is_empty(),
        "a finished run keeps nothing; the answer comes from the durable log"
    );
}

#[tokio::test]
async fn a_reader_left_behind_by_the_byte_budget_is_told_about_the_gap() {
    let (Some(writer), Some(reader)) = (sink().await, sink().await) else {
        return;
    };
    let run = run_id();

    let chunk = "x".repeat(64 * 1024);
    for _ in 0..48 {
        writer.publish(&run, delta(&chunk));
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let replay = within(reader.since(&run, 0)).await;
        if replay.gap {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the window never evicted: still holding from the start"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn a_reader_inside_the_window_sees_no_gap() {
    let (Some(writer), Some(reader)) = (sink().await, sink().await) else {
        return;
    };
    let run = run_id();

    for index in 0..4 {
        writer.publish(&run, delta(&index.to_string()));
    }
    let all = within(reader.since(&run, 0)).await;
    let newest = all.events.last().map(|(seq, _)| *seq).unwrap_or(0);

    let replay = within(reader.since(&run, newest.saturating_sub(1))).await;
    assert!(!replay.gap, "asking for what is still held is not a gap");
}
