//! Property tests for the stores — invariants stated once, checked over many
//! generated inputs. Memory/Local only (fast); Postgres proptests, if wanted,
//! belong in a nightly job per the plan.

use proptest::prelude::*;
use tokio::runtime::Runtime;

use runic_state::SessionEvent;
use runic_substrate::{
    ArtifactSource, ArtifactStore, MemoryArtifactStore, MemorySessionStore, SessionStore,
};
use runic_types::Message;

fn rt() -> Runtime {
    Runtime::new().unwrap()
}

fn msg(text: &str) -> SessionEvent {
    SessionEvent::Message {
        run_id: "r".into(),
        msg: Message::user(text),
        at: chrono::DateTime::from_timestamp_micros(1_700_000_000_000_000).unwrap(),
    }
}

/// Ids built from a deliberately nasty alphabet (slashes, underscores, percent,
/// spaces, unicode) — the characters that break naive path/key encoding.
fn weird_id() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec!['a', 'b', '/', '_', '%', ' ', 'é', '世', '.', '-']),
        1..10,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Appended message text comes back in insertion order, every time.
    #[test]
    fn event_text_order_roundtrips(texts in prop::collection::vec("[a-z ]{0,40}", 0..50)) {
        rt().block_on(async {
            let store = MemorySessionStore::new();
            for t in &texts {
                store.append("t", "s", &msg(t)).await.unwrap();
            }
            let got: Vec<String> = store.read("t", "s").await.unwrap()
                .iter()
                .map(|e| match &e.event {
                    SessionEvent::Message { msg, .. } => msg.content.text_content(),
                    _ => unreachable!(),
                })
                .collect();
            prop_assert_eq!(got, texts);
            Ok(())
        })?;
    }

    /// Paginating with any page size reproduces the full read exactly.
    #[test]
    fn pagination_equals_full_read(n in 0usize..120, page in 1usize..20) {
        rt().block_on(async {
            let store = MemorySessionStore::new();
            for i in 0..n {
                store.append("t", "s", &msg(&format!("m{i}"))).await.unwrap();
            }
            let mut walked = Vec::new();
            let mut after = 0u64;
            loop {
                let batch = store.read_after_limited("t", "s", after, page).await.unwrap();
                if batch.is_empty() { break; }
                after = batch.last().unwrap().seq;
                walked.extend(batch.iter().map(|e| e.seq));
            }
            let full: Vec<u64> = store.read("t", "s").await.unwrap().iter().map(|e| e.seq).collect();
            prop_assert_eq!(walked, full);
            Ok(())
        })?;
    }

    /// Distinct (even confusable) tenant/session ids never bleed into each other.
    #[test]
    fn weird_ids_never_collide(a in weird_id(), b in weird_id(), na in 1usize..6, nb in 1usize..6) {
        prop_assume!(a != b);
        rt().block_on(async {
            let store = MemorySessionStore::new();
            // use the strings as BOTH tenant and session to maximise collision risk
            for _ in 0..na { store.append(&a, &a, &msg("x")).await.unwrap(); }
            for _ in 0..nb { store.append(&b, &b, &msg("y")).await.unwrap(); }
            prop_assert_eq!(store.read(&a, &a).await.unwrap().len(), na);
            prop_assert_eq!(store.read(&b, &b).await.unwrap().len(), nb);
            Ok(())
        })?;
    }

    /// Label updates are last-writer-wins as seen by `session_meta`.
    #[test]
    fn label_is_last_writer(updates in prop::collection::vec(prop::option::of("[a-z]{1,8}"), 1..12)) {
        rt().block_on(async {
            let store = MemorySessionStore::new();
            store.append("t", "s", &msg("seed")).await.unwrap();
            for u in &updates {
                store.set_label("t", "s", u.as_deref()).await.unwrap();
            }
            let got = store.session_meta("t", "s").await.unwrap().unwrap().label;
            prop_assert_eq!(got, updates.last().unwrap().clone());
            Ok(())
        })?;
    }

    /// Artifact put/get/head agree on bytes, size, and mime for arbitrary bytes.
    #[test]
    fn artifact_put_get_head_consistent(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
        rt().block_on(async {
            let store = MemoryArtifactStore::new();
            let a = store.put("t", "s", "application/octet-stream", ArtifactSource::ToolOutput, &bytes).await.unwrap();
            prop_assert_eq!(a.size, bytes.len() as u64);
            prop_assert_eq!(store.get(&a.id).await.unwrap(), bytes);
            let head = store.head(&a.id).await.unwrap();
            prop_assert_eq!(head.id, a.id);
            prop_assert_eq!(head.size, a.size);
            prop_assert_eq!(head.mime_type, "application/octet-stream");
            Ok(())
        })?;
    }

    /// Model check: after a sequence of puts then a subset of deletes, the
    /// store's `list` equals the model's surviving set (Memory honors this).
    #[test]
    fn artifact_delete_matches_model(count in 0usize..30, drops in prop::collection::vec(any::<bool>(), 0..30)) {
        rt().block_on(async {
            let store = MemoryArtifactStore::new();
            let mut ids = Vec::new();
            for i in 0..count {
                let a = store.put("t", "s", "text/plain", ArtifactSource::UserUpload, format!("n{i}").as_bytes()).await.unwrap();
                ids.push(a.id);
            }
            let mut alive: std::collections::HashSet<String> = ids.iter().cloned().collect();
            for (i, drop) in drops.iter().enumerate() {
                if *drop && i < ids.len() {
                    store.delete(&ids[i]).await.unwrap();
                    alive.remove(&ids[i]);
                }
            }
            let listed: std::collections::HashSet<String> =
                store.list("t", "s").await.unwrap().into_iter().map(|a| a.id).collect();
            prop_assert_eq!(listed, alive);
            Ok(())
        })?;
    }
}
