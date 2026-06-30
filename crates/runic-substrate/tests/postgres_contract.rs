//! Postgres backend: the full contract suites + Postgres-specific risks
//! (real concurrency, FK cascade, transactional batch, timestamp precision).
//!
//! Gated two ways:
//!   * compiled only with `--features postgres`;
//!   * each test no-ops unless `RUNIC_TEST_DATABASE_URL` points at a scratch DB.
//!
//! Run with, e.g.:
//!   RUNIC_TEST_DATABASE_URL=postgres://localhost/runic_test \
//!     cargo test --features postgres --test postgres_contract
#![cfg(feature = "postgres")]

mod common;

use std::sync::Arc;

use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use runic_state::SessionEvent;
use runic_substrate::{
    ArtifactStore, Error, MemoryArtifactStore, PostgresArtifactStore, PostgresSessionStore,
    SessionStore,
};
use runic_types::Message;

use crate::common::ids::tenant_session;

/// A small per-test pool from the env URL — `None` when no test DB is set, which
/// makes every generated case skip cleanly. Per-test (not shared) pools keep one
/// test's 20-task concurrency burst from starving the rest; migrations stay safe
/// across pools via the advisory lock in `migrate`. A loud one-time stderr
/// notice keeps "0 ran" from being mistaken for "Postgres verified".
async fn test_pool() -> Option<PgPool> {
    let Ok(url) = std::env::var("RUNIC_TEST_DATABASE_URL") else {
        static NOTED: AtomicBool = AtomicBool::new(false);
        if !NOTED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "\n⚠  RUNIC_TEST_DATABASE_URL not set — postgres_contract tests SKIPPED \
                 (NOT verified). Run scripts/test-postgres.sh to verify.\n"
            );
        }
        return None;
    };
    Some(
        PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .expect("RUNIC_TEST_DATABASE_URL is set but unreachable"),
    )
}

async fn pg_sessions() -> Option<PostgresSessionStore> {
    let pool = test_pool().await?;
    Some(PostgresSessionStore::from_pool(pool).await.unwrap())
}

async fn pg_artifacts() -> Option<PostgresArtifactStore> {
    let pool = test_pool().await?;
    let bytes: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifactStore::new());
    Some(
        PostgresArtifactStore::from_pool(pool, bytes, "memory")
            .await
            .unwrap(),
    )
}

session_store_contract_suite!(|| async { pg_sessions().await });
session_store_search_suite!(|| async { pg_sessions().await });
artifact_store_contract_suite!(|| async { pg_artifacts().await });
// Postgres DOES satisfy the stronger delete-from-list guarantee.
artifact_store_delete_from_list_suite!(|| async { pg_artifacts().await });
session_store_stress_suite!(|| async { pg_sessions().await });
artifact_store_stress_suite!(|| async { pg_artifacts().await });

// ── Postgres-specific ─────────────────────────────────────────────────────────

fn msg(text: &str, at: DateTime<Utc>) -> SessionEvent {
    SessionEvent::Message {
        run_id: "r".into(),
        msg: Message::user(text),
        at,
    }
}

/// The production builders must fail closed on a bad URL — never silently fall
/// back to a different backend. (Runs without a test DB: the URL is bogus.)
#[tokio::test]
async fn production_builders_fail_closed_on_bad_url() {
    let bad = "postgres://nobody:nobody@127.0.0.1:1/none";
    assert!(runic_substrate::sessions_postgres(bad).await.is_err());
    assert!(
        runic_substrate::blobs_postgres(bad, std::env::temp_dir().join("runic-fail-closed"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn migrations_apply_and_are_idempotent() {
    let Some(pool) = test_pool().await else {
        return;
    };
    // Two independent migrate passes over the same DB must both succeed.
    PostgresSessionStore::from_pool(pool.clone()).await.unwrap();
    PostgresSessionStore::from_pool(pool).await.unwrap();
}

/// Deleting a session cascades its event-log rows AND its full-text projection.
#[tokio::test]
async fn delete_session_cascades_events_and_search_index() {
    let Some(store) = pg_sessions().await else {
        return;
    };
    let (t, s) = tenant_session();
    store
        .append(&t, &s, &msg("cascade target word", Utc::now()))
        .await
        .unwrap();
    assert!(
        !store
            .search(&t, "cascade", 10, None)
            .await
            .unwrap()
            .is_empty()
    );

    store.delete_session(&t, &s).await.unwrap();
    assert!(store.read(&t, &s).await.unwrap().is_empty());
    assert!(
        store
            .search(&t, "cascade", 10, None)
            .await
            .unwrap()
            .is_empty(),
        "chat_messages row must cascade with the session"
    );
}

/// Real cross-task contention on one session: the row-locked seq counter must
/// hand out a unique, gapless seq to every committed append.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_same_session_unique_monotonic_seq() {
    let Some(store) = pg_sessions().await else {
        return;
    };
    let store = Arc::new(store);
    let (t, s) = tenant_session();
    let barrier = Arc::new(tokio::sync::Barrier::new(20));
    let mut set = tokio::task::JoinSet::new();
    for i in 0..20 {
        let (store, barrier, t, s) = (store.clone(), barrier.clone(), t.clone(), s.clone());
        set.spawn(async move {
            barrier.wait().await;
            store
                .append(&t, &s, &msg(&format!("e{i}"), Utc::now()))
                .await
                .unwrap();
        });
    }
    while let Some(r) = set.join_next().await {
        r.unwrap();
    }
    let mut seqs: Vec<u64> = store
        .read(&t, &s)
        .await
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    seqs.sort();
    assert_eq!(
        seqs,
        (1..=20).collect::<Vec<_>>(),
        "concurrent appends lost/duplicated a seq"
    );
}

/// Two concurrent batches on one session interleave into a single contiguous,
/// gapless seq space — neither batch corrupts the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_batches_same_session_stay_contiguous() {
    let Some(store) = pg_sessions().await else {
        return;
    };
    let store = Arc::new(store);
    let (t, s) = tenant_session();
    let batch_a: Vec<SessionEvent> = (0..10).map(|i| msg(&format!("a{i}"), Utc::now())).collect();
    let batch_b: Vec<SessionEvent> = (0..10).map(|i| msg(&format!("b{i}"), Utc::now())).collect();

    let h1 = tokio::spawn({
        let (store, t, s) = (store.clone(), t.clone(), s.clone());
        async move { store.append_batch(&t, &s, &batch_a).await.unwrap() }
    });
    let h2 = tokio::spawn({
        let (store, t, s) = (store.clone(), t.clone(), s.clone());
        async move { store.append_batch(&t, &s, &batch_b).await.unwrap() }
    });
    h1.await.unwrap();
    h2.await.unwrap();

    let mut seqs: Vec<u64> = store
        .read(&t, &s)
        .await
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    seqs.sort();
    assert_eq!(seqs, (1..=20).collect::<Vec<_>>());
}

/// `sessions.last_activity` is a `TIMESTAMPTZ` → microsecond precision. Sub-µs
/// nanos in the event timestamp are truncated there (the event JSONB keeps the
/// full value; this asserts the indexed column's documented precision).
#[tokio::test]
async fn metadata_timestamp_is_microsecond_precision() {
    let Some(store) = pg_sessions().await else {
        return;
    };
    let (t, s) = tenant_session();
    let at = DateTime::from_timestamp(1_700_000_000, 123_456_789).unwrap(); // ns precision
    store.append(&t, &s, &msg("x", at)).await.unwrap();
    let meta = store.session_meta(&t, &s).await.unwrap().unwrap();
    assert_eq!(
        meta.last_activity.timestamp_subsec_nanos() % 1000,
        0,
        "TIMESTAMPTZ should be truncated to microseconds"
    );
}

/// A NUL byte in message text cannot be stored in Postgres text/jsonb. The
/// store must fail closed (a typed error, no panic) — never silently drop it.
#[tokio::test]
async fn nul_byte_text_fails_closed() {
    let Some(store) = pg_sessions().await else {
        return;
    };
    let (t, s) = tenant_session();
    let r = store.append(&t, &s, &msg("bad\u{0}nul", Utc::now())).await;
    assert!(
        matches!(r, Err(Error::Database(_))),
        "NUL must error, got {r:?}"
    );
    // and nothing partial was committed
    assert!(store.read(&t, &s).await.unwrap().is_empty());
}

/// A batch whose later event hits a DB error rolls back wholesale: no partial
/// event rows, no partial search-index rows, no advanced sequence — and a retry
/// afterwards still works from seq 1.
#[tokio::test]
async fn batch_rollback_leaves_no_partial_rows() {
    let Some(store) = pg_sessions().await else {
        return;
    };
    let (t, s) = tenant_session();
    // 2nd event carries a NUL → the chat_messages/jsonb insert fails mid-batch.
    let bad = vec![
        msg("good searchable word", Utc::now()),
        msg("poison\u{0}pill", Utc::now()),
    ];
    assert!(store.append_batch(&t, &s, &bad).await.is_err());

    assert!(
        store.read(&t, &s).await.unwrap().is_empty(),
        "partial event rows survived rollback"
    );
    assert!(
        store
            .search(&t, "searchable", 10, None)
            .await
            .unwrap()
            .is_empty(),
        "partial search-index rows survived rollback"
    );

    // sequence state is intact: a fresh append starts at 1
    let seq = store
        .append(&t, &s, &msg("retry", Utc::now()))
        .await
        .unwrap();
    assert_eq!(seq, 1, "rolled-back batch must not advance the sequence");
}

// ── full-text search semantics (Postgres-only; websearch_to_tsquery) ──────────

async fn seed(store: &PostgresSessionStore, t: &str, s: &str, text: &str) {
    store.append(t, s, &msg(text, Utc::now())).await.unwrap();
}

#[tokio::test]
async fn search_supports_phrase_terms_or_and_negation() {
    let Some(store) = pg_sessions().await else {
        return;
    };
    let t = crate::common::ids::uid("tenant");
    seed(
        &store,
        &t,
        &crate::common::ids::uid("s"),
        "the quick brown fox jumps",
    )
    .await;

    // quoted phrase: adjacent-in-order matches, reversed does not
    assert!(
        !store
            .search(&t, "\"quick brown\"", 10, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .search(&t, "\"brown quick\"", 10, None)
            .await
            .unwrap()
            .is_empty()
    );
    // multiple bare terms = AND
    assert!(
        !store
            .search(&t, "quick fox", 10, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .search(&t, "quick zebra", 10, None)
            .await
            .unwrap()
            .is_empty()
    );
    // OR
    assert!(
        !store
            .search(&t, "zebra or fox", 10, None)
            .await
            .unwrap()
            .is_empty()
    );
    // negation
    assert!(
        !store
            .search(&t, "quick -zebra", 10, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .search(&t, "quick -fox", 10, None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn search_ranks_more_relevant_first() {
    let Some(store) = pg_sessions().await else {
        return;
    };
    let t = crate::common::ids::uid("tenant");
    let (weak, strong) = (
        crate::common::ids::uid("weak"),
        crate::common::ids::uid("strong"),
    );
    seed(&store, &t, &weak, "deployment mentioned once here").await;
    seed(
        &store,
        &t,
        &strong,
        "deployment deployment deployment deployment",
    )
    .await;
    let hits = store.search(&t, "deployment", 10, None).await.unwrap();
    assert!(hits.len() >= 2);
    assert_eq!(hits[0].session_id, strong, "higher ts_rank must sort first");
}

// ── schema contract: the shape the code relies on actually exists ─────────────

#[tokio::test]
async fn schema_has_required_columns_indexes_and_fks() {
    let Some(pool) = test_pool().await else {
        return;
    };

    // key column types
    let col_type = |table: &'static str, col: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT data_type FROM information_schema.columns
                 WHERE table_name = $1 AND column_name = $2",
            )
            .bind(table)
            .bind(col)
            .fetch_optional(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(
        col_type("session_events", "event").await.as_deref(),
        Some("jsonb")
    );
    assert_eq!(
        col_type("session_events", "seq").await.as_deref(),
        Some("bigint")
    );
    assert_eq!(
        col_type("chat_messages", "tsv").await.as_deref(),
        Some("tsvector")
    );
    assert_eq!(
        col_type("artifacts", "size").await.as_deref(),
        Some("bigint")
    );

    // the GIN full-text index the search query depends on
    let gin: Option<String> = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes WHERE indexname = 'chat_messages_tsv_idx'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(
        gin.is_some_and(|d| d.contains("gin")),
        "chat_messages GIN index missing"
    );

    // artifacts → sessions foreign key exists
    let fk: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.table_constraints
         WHERE table_name = 'artifacts' AND constraint_type = 'FOREIGN KEY'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(fk >= 1, "artifacts must FK to sessions");
}
