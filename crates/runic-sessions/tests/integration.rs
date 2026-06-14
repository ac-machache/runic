//! Integration tests for runic-sessions — exercise SessionStore +
//! FileSessionStore + spawn_persister + replay against a real
//! StorageBackend (in-memory for speed, FS via tempdir for the
//! append-real-fs test).

use chrono::Utc;
use runic_agent_core::{
    HookLifecycle, RunOutcome, SessionEvent, EVENT_BROADCAST_CAPACITY,
};
use runic_message_types::Message;
use runic_sessions::{
    replay_into_state, replay_messages, spawn_persister, FileSessionStore, SessionStore,
    StoreError,
};
use runic_storage_backend::{LocalFsBackend, MemoryBackend, StorageBackend};
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::broadcast;

fn fresh_store() -> Arc<dyn SessionStore> {
    let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
    Arc::new(FileSessionStore::new(storage))
}

fn run_start(run_id: &str) -> SessionEvent {
    SessionEvent::RunStart {
        run_id: run_id.into(),
        at: Utc::now(),
    }
}

fn message(run_id: &str, text: &str) -> SessionEvent {
    SessionEvent::Message {
        run_id: run_id.into(),
        msg: Message::user(text),
        at: Utc::now(),
    }
}

fn assistant_message(run_id: &str, text: &str) -> SessionEvent {
    SessionEvent::Message {
        run_id: run_id.into(),
        msg: Message::assistant_text(text),
        at: Utc::now(),
    }
}

fn run_end(run_id: &str) -> SessionEvent {
    SessionEvent::RunEnd {
        run_id: run_id.into(),
        outcome: RunOutcome {
            total_turns: 1,
            stop_reason: Some("end_turn".into()),
            usage: Default::default(),
            structured_result: None,
        },
        at: Utc::now(),
    }
}

// ─── Round-trips ────────────────────────────────────────────────────────────

#[tokio::test]
async fn append_then_read_returns_events_in_seq_order() {
    let store = fresh_store();
    let s1 = store.append("alice", "sess1", &run_start("r1")).await.unwrap();
    let s2 = store
        .append("alice", "sess1", &message("r1", "hi"))
        .await
        .unwrap();
    let s3 = store
        .append("alice", "sess1", &assistant_message("r1", "hello"))
        .await
        .unwrap();
    let s4 = store.append("alice", "sess1", &run_end("r1")).await.unwrap();

    assert_eq!(s1, 1);
    assert_eq!(s2, 2);
    assert_eq!(s3, 3);
    assert_eq!(s4, 4);

    let read = store.read("alice", "sess1").await.unwrap();
    assert_eq!(read.len(), 4);
    let seqs: Vec<u64> = read.iter().map(|s| s.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn seq_continues_across_store_instances() {
    // Simulating process restart: two FileSessionStores on the same
    // storage backend should agree on seq numbering.
    let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());

    let s1 = Arc::new(FileSessionStore::new(storage.clone()));
    let _ = s1.append("alice", "sess", &run_start("r1")).await.unwrap();
    let _ = s1
        .append("alice", "sess", &message("r1", "hi"))
        .await
        .unwrap();
    drop(s1);

    let s2 = Arc::new(FileSessionStore::new(storage.clone()));
    let next = s2
        .append("alice", "sess", &assistant_message("r1", "hello"))
        .await
        .unwrap();
    assert_eq!(next, 3, "seq must resume from the on-disk max");

    let all = s2.read("alice", "sess").await.unwrap();
    assert_eq!(all.len(), 3);
}

// ─── Tenant isolation ───────────────────────────────────────────────────────

#[tokio::test]
async fn tenants_are_isolated_in_reads_and_lists() {
    let store = fresh_store();
    store
        .append("alice", "sess-a", &run_start("r1"))
        .await
        .unwrap();
    store
        .append("bob", "sess-b", &run_start("r2"))
        .await
        .unwrap();
    store
        .append("alice", "sess-a", &message("r1", "alice's message"))
        .await
        .unwrap();
    store
        .append("bob", "sess-b", &message("r2", "bob's message"))
        .await
        .unwrap();

    let alice = store.read("alice", "sess-a").await.unwrap();
    let bob = store.read("bob", "sess-b").await.unwrap();
    assert_eq!(alice.len(), 2);
    assert_eq!(bob.len(), 2);

    // Alice should NOT be able to read bob's session even with the right id.
    let cross = store.read("alice", "sess-b").await.unwrap();
    assert!(cross.is_empty());

    let alice_sessions = store.list_sessions("alice").await.unwrap();
    assert_eq!(alice_sessions, vec!["sess-a"]);
    let bob_sessions = store.list_sessions("bob").await.unwrap();
    assert_eq!(bob_sessions, vec!["sess-b"]);
}

#[tokio::test]
async fn seq_is_per_tenant_per_session() {
    let store = fresh_store();
    let a1 = store
        .append("alice", "sess-1", &run_start("r1"))
        .await
        .unwrap();
    let a2 = store
        .append("alice", "sess-2", &run_start("r2"))
        .await
        .unwrap();
    let b1 = store
        .append("bob", "sess-1", &run_start("r3"))
        .await
        .unwrap();

    // Each (tenant, session) gets its own sequence starting at 1.
    assert_eq!(a1, 1);
    assert_eq!(a2, 1);
    assert_eq!(b1, 1);
}

// ─── Tailing via read_after ────────────────────────────────────────────────

#[tokio::test]
async fn read_after_returns_only_newer_events() {
    let store = fresh_store();
    let _ = store.append("t", "s", &run_start("r1")).await.unwrap();
    let _ = store.append("t", "s", &message("r1", "a")).await.unwrap();
    let _ = store.append("t", "s", &message("r1", "b")).await.unwrap();
    let _ = store.append("t", "s", &message("r1", "c")).await.unwrap();

    let after_2 = store.read_after("t", "s", 2).await.unwrap();
    assert_eq!(after_2.len(), 2);
    assert_eq!(after_2[0].seq, 3);
    assert_eq!(after_2[1].seq, 4);

    let after_all = store.read_after("t", "s", 100).await.unwrap();
    assert!(after_all.is_empty());
}

// ─── Delete ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_session_removes_all_events_and_is_idempotent() {
    let store = fresh_store();
    store.append("t", "s", &run_start("r1")).await.unwrap();
    store.append("t", "s", &message("r1", "x")).await.unwrap();
    assert_eq!(store.read("t", "s").await.unwrap().len(), 2);

    store.delete_session("t", "s").await.unwrap();
    assert!(store.read("t", "s").await.unwrap().is_empty());

    // Idempotent — second delete is fine.
    store.delete_session("t", "s").await.unwrap();
    store.delete_session("t", "does-not-exist").await.unwrap();
}

#[tokio::test]
async fn delete_resets_seq_for_the_session() {
    let store = fresh_store();
    let _ = store.append("t", "s", &run_start("r1")).await.unwrap();
    let _ = store.append("t", "s", &message("r1", "x")).await.unwrap();

    store.delete_session("t", "s").await.unwrap();

    // After delete, seq starts fresh.
    let next = store.append("t", "s", &run_start("r2")).await.unwrap();
    assert_eq!(next, 1);
}

// ─── Listing ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_sessions_for_empty_tenant_is_empty() {
    let store = fresh_store();
    let none = store.list_sessions("ghost").await.unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn list_sessions_returns_sorted() {
    let store = fresh_store();
    store.append("t", "zeta", &run_start("r1")).await.unwrap();
    store.append("t", "alpha", &run_start("r2")).await.unwrap();
    store.append("t", "mu", &run_start("r3")).await.unwrap();
    let sessions = store.list_sessions("t").await.unwrap();
    assert_eq!(sessions, vec!["alpha", "mu", "zeta"]);
}

#[tokio::test]
async fn list_tenants_returns_all_known_tenants() {
    let store = fresh_store();
    store.append("alice", "s1", &run_start("r1")).await.unwrap();
    store.append("bob", "s1", &run_start("r1")).await.unwrap();
    store.append("carol", "s1", &run_start("r1")).await.unwrap();
    let tenants = store.list_tenants().await.unwrap();
    assert_eq!(tenants, vec!["alice", "bob", "carol"]);
}

// ─── Persister integration ─────────────────────────────────────────────────

#[tokio::test]
async fn persister_drains_broadcast_into_store() {
    let store = fresh_store();
    let (tx, rx) = broadcast::channel::<SessionEvent>(EVENT_BROADCAST_CAPACITY);

    let handle = spawn_persister(rx, store.clone(), "t".into(), "s".into());

    tx.send(run_start("r1")).unwrap();
    tx.send(message("r1", "hi")).unwrap();
    tx.send(assistant_message("r1", "hello")).unwrap();
    tx.send(run_end("r1")).unwrap();

    drop(tx); // closes channel → persister exits naturally
    handle.join().await.unwrap();

    let read = store.read("t", "s").await.unwrap();
    assert_eq!(read.len(), 4);
}

#[tokio::test]
async fn persister_keeps_going_after_store_errors() {
    // A store that fails every other append. The persister should log
    // and continue, NOT crash.
    use async_trait::async_trait;
    use runic_sessions::{SessionStore as Trait, StoredEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FlakyStore {
        call_count: AtomicUsize,
    }
    #[async_trait]
    impl Trait for FlakyStore {
        async fn append(
            &self,
            _t: &str,
            _s: &str,
            _e: &SessionEvent,
        ) -> Result<u64, StoreError> {
            let n = self.call_count.fetch_add(1, Ordering::SeqCst);
            if n.is_multiple_of(2) {
                Err(StoreError::Storage("simulated".into()))
            } else {
                Ok(n as u64)
            }
        }
        async fn read(&self, _t: &str, _s: &str) -> Result<Vec<StoredEvent>, StoreError> {
            Ok(Vec::new())
        }
        async fn read_after(
            &self,
            _t: &str,
            _s: &str,
            _a: u64,
        ) -> Result<Vec<StoredEvent>, StoreError> {
            Ok(Vec::new())
        }
        async fn list_sessions(&self, _t: &str) -> Result<Vec<String>, StoreError> {
            Ok(Vec::new())
        }
        async fn delete_session(&self, _t: &str, _s: &str) -> Result<(), StoreError> {
            Ok(())
        }
    }

    let flaky: Arc<dyn SessionStore> = Arc::new(FlakyStore::default());
    let (tx, rx) = broadcast::channel::<SessionEvent>(EVENT_BROADCAST_CAPACITY);
    let handle = spawn_persister(rx, flaky.clone(), "t".into(), "s".into());

    for _ in 0..6 {
        tx.send(run_start("r")).unwrap();
    }
    drop(tx);
    handle.join().await.unwrap();
    // If we got here, the persister survived all the failures.
}

// ─── Replay ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn replay_into_state_rebuilds_full_event_history() {
    let store = fresh_store();
    store.append("t", "s", &run_start("r1")).await.unwrap();
    store.append("t", "s", &message("r1", "hi")).await.unwrap();
    store
        .append("t", "s", &assistant_message("r1", "hello"))
        .await
        .unwrap();
    store.append("t", "s", &run_end("r1")).await.unwrap();

    let state = replay_into_state(&*store, "t", "s", "you are an assistant")
        .await
        .unwrap();
    assert_eq!(state.session_id, "s");
    assert_eq!(state.system_prompt, "you are an assistant");
    assert_eq!(state.events.len(), 4);

    // Derived view should give us back the user + assistant messages.
    let msgs = state.messages_for_provider();
    assert_eq!(msgs.len(), 2);
}

#[tokio::test]
async fn replay_messages_extracts_just_message_blocks() {
    let store = fresh_store();
    store.append("t", "s", &run_start("r1")).await.unwrap();
    store.append("t", "s", &message("r1", "hi")).await.unwrap();
    store
        .append("t", "s", &assistant_message("r1", "hello"))
        .await
        .unwrap();
    store
        .append(
            "t",
            "s",
            &SessionEvent::HookRan {
                run_id: "r1".into(),
                hook: "logging".into(),
                lifecycle: HookLifecycle::BeforeModel,
                note: None,
                at: Utc::now(),
            },
        )
        .await
        .unwrap();
    store.append("t", "s", &run_end("r1")).await.unwrap();

    let msgs = replay_messages(&*store, "t", "s").await.unwrap();
    // Two real messages; the hook event was filtered out.
    assert_eq!(msgs.len(), 2);
}

// ─── Real filesystem ───────────────────────────────────────────────────────

#[tokio::test]
async fn round_trip_against_real_localfs_backend() {
    let dir = tempdir().unwrap();
    let storage: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(dir.path()));
    let store: Arc<dyn SessionStore> = Arc::new(FileSessionStore::new(storage));

    for i in 0..10 {
        let _ = store
            .append("alice", "thread-x", &message("r1", &format!("msg {i}")))
            .await
            .unwrap();
    }

    let read = store.read("alice", "thread-x").await.unwrap();
    assert_eq!(read.len(), 10);
    for (i, ev) in read.iter().enumerate() {
        assert_eq!(ev.seq, (i + 1) as u64);
    }
}
