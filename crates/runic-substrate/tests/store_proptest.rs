//! Property tests for the event-sourced session store — the persistence
//! invariants the whole replay/resume story rests on: `append` hands back a
//! strictly-increasing `seq` per `(tenant, session)`, `read` is seq-ordered,
//! `read_after` is an exclusive tail, and tenants/sessions are isolated.
//!
//! Async, so each generated case runs on a small current-thread runtime.

use chrono::{DateTime, Utc};
use proptest::prelude::*;
use tokio::runtime::Runtime;

use runic_state::SessionEvent;
use runic_substrate::{MemorySessionStore, SessionStore};
use runic_types::Message;

fn ts() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()
}

/// A trivial message event — content is irrelevant to the seq invariants.
fn ev(i: usize) -> SessionEvent {
    SessionEvent::Message {
        run_id: "r-1".into(),
        msg: Message::user(format!("m{i}")),
        at: ts(),
    }
}

proptest! {
    /// `append` returns strictly increasing seqs, and `read` returns exactly
    /// those events in seq order.
    #[test]
    fn append_assigns_monotonic_seq(n in 0usize..40) {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let store = MemorySessionStore::new();
            let mut seqs = Vec::new();
            for i in 0..n {
                seqs.push(store.append("t", "s", &ev(i)).await.unwrap());
            }
            // strictly increasing
            for w in seqs.windows(2) {
                prop_assert!(w[1] > w[0], "seqs not strictly increasing: {:?}", w);
            }
            // read returns all, in seq order
            let read = store.read("t", "s").await.unwrap();
            prop_assert_eq!(read.len(), n);
            let read_seqs: Vec<u64> = read.iter().map(|e| e.seq).collect();
            let mut sorted = read_seqs.clone();
            sorted.sort();
            prop_assert_eq!(read_seqs, sorted);
            Ok(())
        })?;
    }

    /// `read_after(k)` is an exclusive tail — exactly the events with `seq > k`.
    #[test]
    fn read_after_is_exclusive_tail(n in 1usize..40, cut in 0u64..40) {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let store = MemorySessionStore::new();
            for i in 0..n {
                store.append("t", "s", &ev(i)).await.unwrap();
            }
            let tail = store.read_after("t", "s", cut).await.unwrap();
            prop_assert!(tail.iter().all(|e| e.seq > cut));
            let expected = store.read("t", "s").await.unwrap().iter().filter(|e| e.seq > cut).count();
            prop_assert_eq!(tail.len(), expected);
            Ok(())
        })?;
    }

    /// Sessions and tenants are isolated — events under one key never leak into
    /// another.
    #[test]
    fn sessions_and_tenants_are_isolated(a in 0usize..15, b in 0usize..15) {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let store = MemorySessionStore::new();
            for i in 0..a { store.append("alice", "s1", &ev(i)).await.unwrap(); }
            for i in 0..b { store.append("bob", "s1", &ev(i)).await.unwrap(); }
            for i in 0..b { store.append("alice", "s2", &ev(i)).await.unwrap(); }

            prop_assert_eq!(store.read("alice", "s1").await.unwrap().len(), a);
            prop_assert_eq!(store.read("bob", "s1").await.unwrap().len(), b);
            prop_assert_eq!(store.read("alice", "s2").await.unwrap().len(), b);
            // an untouched key is empty
            prop_assert!(store.read("carol", "s1").await.unwrap().is_empty());
            Ok(())
        })?;
    }

    /// `delete_session` clears a session and only that session.
    #[test]
    fn delete_clears_only_the_target(n in 1usize..30) {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let store = MemorySessionStore::new();
            for i in 0..n {
                store.append("t", "keep", &ev(i)).await.unwrap();
                store.append("t", "drop", &ev(i)).await.unwrap();
            }
            store.delete_session("t", "drop").await.unwrap();
            prop_assert!(store.read("t", "drop").await.unwrap().is_empty());
            prop_assert_eq!(store.read("t", "keep").await.unwrap().len(), n);
            Ok(())
        })?;
    }
}
