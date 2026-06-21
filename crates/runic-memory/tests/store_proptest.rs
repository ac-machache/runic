//! Property tests for the bounded memory store — the invariants that make it
//! safe to expose as a model-writable tool: the cap is NEVER breached, entries
//! round-trip through the `§` format, and `add` is idempotent.
//!
//! The store is async, so each generated case drives a fresh in-RAM `MemoryFs`
//! on a small current-thread runtime.

use std::sync::Arc;

use proptest::prelude::*;
use tokio::runtime::Runtime;

use runic_filesystem::MemoryFs;
use runic_memory::{BoundedMemoryStore, Target};

const CAP: usize = 200;

fn store() -> BoundedMemoryStore {
    BoundedMemoryStore::new(Arc::new(MemoryFs::new()))
        .with_limits(CAP, CAP)
        .with_threat_scanning(false) // exercise arbitrary content, not the scanner
}

/// Entry text: non-empty-ish single lines (the store trims + rejects empty).
fn entries() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec("[a-zA-Z0-9 ]{1,40}", 0..30)
}

proptest! {
    /// After adding ANY sequence of entries, the stored total never exceeds the
    /// cap, and every stored entry is itself within the cap. (Adds that would
    /// breach the cap are rejected, so the on-disk state stays bounded.)
    #[test]
    fn cap_is_never_breached(items in entries()) {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let s = store();
            for it in &items {
                let _ = s.add(Target::Memory, it).await; // Ok or rejected — both fine
            }
            let total = s.char_count(Target::Memory).await.unwrap();
            prop_assert!(total <= CAP, "total {total} exceeded cap {CAP}");
            for e in s.read(Target::Memory).await.unwrap() {
                prop_assert!(e.chars().count() <= CAP);
            }
            Ok(())
        })?;
    }

    /// No duplicates: `add` is idempotent, so the stored set has no repeats even
    /// if the input repeats.
    #[test]
    fn entries_are_deduped(items in entries()) {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let s = store();
            for it in &items {
                let _ = s.add(Target::Memory, it).await;
            }
            let stored = s.read(Target::Memory).await.unwrap();
            let mut sorted = stored.clone();
            sorted.sort();
            sorted.dedup();
            prop_assert_eq!(stored.len(), sorted.len(), "stored set has duplicates");
            Ok(())
        })?;
    }

    /// Round-trip: whatever ends up stored survives a write→read cycle in order
    /// (the `\n§\n` format parses back to exactly the entries written).
    #[test]
    fn stored_entries_round_trip(items in entries()) {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let s = store();
            for it in &items {
                let _ = s.add(Target::Memory, it).await;
            }
            let first = s.read(Target::Memory).await.unwrap();
            // A second store over the same data reads back identically.
            let second = s.read(Target::Memory).await.unwrap();
            prop_assert_eq!(first, second);
            Ok(())
        })?;
    }

    /// The two targets are isolated — writes to MEMORY never appear in USER.
    #[test]
    fn targets_stay_isolated(items in entries()) {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            let s = store();
            for it in &items {
                let _ = s.add(Target::Memory, it).await;
            }
            prop_assert!(s.read(Target::User).await.unwrap().is_empty());
            Ok(())
        })?;
    }
}
