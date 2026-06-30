//! Memory backend: the full contract suites + memory-specific risks
//! (aliasing, true multi-task concurrency, snapshot stability).

mod common;

use std::sync::Arc;

use runic_state::SessionEvent;
use runic_substrate::{
    ArtifactSource, ArtifactStore, MemoryArtifactStore, MemorySessionStore, SessionStore,
};
use runic_types::Message;

session_store_contract_suite!(|| async { Some(MemorySessionStore::new()) });
session_store_search_suite!(|| async { Some(MemorySessionStore::new()) });
artifact_store_contract_suite!(|| async { Some(MemoryArtifactStore::new()) });
artifact_store_delete_from_list_suite!(|| async { Some(MemoryArtifactStore::new()) });
session_store_stress_suite!(|| async { Some(MemorySessionStore::new()) });
artifact_store_stress_suite!(|| async { Some(MemoryArtifactStore::new()) });

fn msg(text: &str) -> SessionEvent {
    SessionEvent::Message {
        run_id: "r".into(),
        msg: Message::user(text),
        at: chrono::Utc::now(),
    }
}

/// The in-RAM backend has no text encoding to satisfy, so NUL round-trips
/// (unlike Postgres — see `postgres_contract::nul_byte_text_fails_closed`).
#[tokio::test]
async fn nul_byte_text_roundtrips_in_memory() {
    let store = MemorySessionStore::new();
    store.append("t", "s", &msg("a\u{0}b")).await.unwrap();
    match &store.read("t", "s").await.unwrap()[0].event {
        SessionEvent::Message { msg, .. } => assert_eq!(msg.content.text_content(), "a\u{0}b"),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn independent_instances_do_not_share_data() {
    let a = MemorySessionStore::new();
    let b = MemorySessionStore::new();
    a.append("t", "s", &msg("only in a")).await.unwrap();
    assert_eq!(a.read("t", "s").await.unwrap().len(), 1);
    assert!(b.read("t", "s").await.unwrap().is_empty());
}

/// Returned events are owned clones — mutating the returned Vec can't corrupt
/// the store.
#[tokio::test]
async fn returned_events_are_owned_clones() {
    let s = MemorySessionStore::new();
    s.append("t", "s", &msg("x")).await.unwrap();
    let mut first = s.read("t", "s").await.unwrap();
    first.clear();
    assert_eq!(
        s.read("t", "s").await.unwrap().len(),
        1,
        "store must not alias returned data"
    );
}

/// Bytes handed back by the artifact store are an owned copy, not a shared
/// reference the caller could mutate underneath the store.
#[tokio::test]
async fn artifact_bytes_are_owned_copies() {
    let store = MemoryArtifactStore::new();
    let a = store
        .put(
            "t",
            "s",
            "application/octet-stream",
            ArtifactSource::UserUpload,
            b"abc",
        )
        .await
        .unwrap();
    let mut got = store.get(&a.id).await.unwrap();
    got[0] = b'Z';
    assert_eq!(
        store.get(&a.id).await.unwrap(),
        b"abc",
        "store bytes must be immutable to callers"
    );
}

/// 20 tasks hammering one session lose no events and produce a unique, gapless
/// seq set. (Mutex-backed, but the contract is the point.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_same_session_no_loss() {
    let store = Arc::new(MemorySessionStore::new());
    let barrier = Arc::new(tokio::sync::Barrier::new(20));
    let mut set = tokio::task::JoinSet::new();
    for i in 0..20 {
        let store = store.clone();
        let barrier = barrier.clone();
        set.spawn(async move {
            barrier.wait().await;
            store
                .append("t", "s", &msg(&format!("e{i}")))
                .await
                .unwrap();
        });
    }
    while let Some(r) = set.join_next().await {
        r.unwrap();
    }
    let seqs: Vec<u64> = store
        .read("t", "s")
        .await
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    let mut sorted = seqs.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        (1..=20).collect::<Vec<_>>(),
        "lost/duplicated events under contention"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_different_tenants_isolated() {
    let store = Arc::new(MemorySessionStore::new());
    let mut set = tokio::task::JoinSet::new();
    for i in 0..20 {
        let store = store.clone();
        set.spawn(async move {
            let t = format!("tenant{i}");
            store.append(&t, "s", &msg("x")).await.unwrap();
        });
    }
    while let Some(r) = set.join_next().await {
        r.unwrap();
    }
    for i in 0..20 {
        assert_eq!(
            store.read(&format!("tenant{i}"), "s").await.unwrap().len(),
            1
        );
    }
}
