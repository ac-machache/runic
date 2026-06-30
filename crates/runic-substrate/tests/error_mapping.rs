//! Error-mapping + trait-default-method coverage.
//!
//! Two things no shipped backend exercises:
//!   * the documented "not found vs empty" split, and that error `Display`
//!     never leaks payload bytes;
//!   * `SessionStore`'s DEFAULT method bodies — Memory/Postgres override
//!     `search`/`cleanup_stale`/`list_tenants`, so a minimal store is the only
//!     way to prove the defaults (and the optional ops returning `Unsupported`).

use async_trait::async_trait;
use chrono::Utc;

use runic_state::SessionEvent;
use runic_substrate::{
    ArtifactSource, ArtifactStore, Error, MemoryArtifactStore, MemorySessionStore, Result,
    SessionMeta, SessionStore, StoredEvent,
};
use runic_types::Message;

// ── not-found vs empty, and no payload leakage ────────────────────────────────

#[tokio::test]
async fn unknown_session_reads_empty_not_error() {
    let s = MemorySessionStore::new();
    assert!(s.read("t", "ghost").await.unwrap().is_empty());
    assert!(s.read_after("t", "ghost", 0).await.unwrap().is_empty());
    assert!(s.session_meta("t", "ghost").await.unwrap().is_none());
}

#[tokio::test]
async fn unknown_artifact_is_notfound_with_id_context() {
    let s = MemoryArtifactStore::new();
    let err = s.get("art-missing").await.unwrap_err();
    assert!(matches!(err, Error::NotFound(_)));
    // context preserved: the id is safe to surface
    assert!(err.to_string().contains("art-missing"));
}

#[tokio::test]
async fn error_display_never_leaks_artifact_bytes() {
    // A secret-looking payload must never appear in any error message.
    let store = MemoryArtifactStore::new();
    let secret = b"SUPER_SECRET_TOKEN_abc123";
    let a = store
        .put("t", "s", "text/plain", ArtifactSource::UserUpload, secret)
        .await
        .unwrap();
    store.delete(&a.id).await.unwrap();
    let err = store.get(&a.id).await.unwrap_err();
    assert!(
        !err.to_string().contains("SUPER_SECRET_TOKEN"),
        "bytes leaked into error text"
    );
}

// ── trait defaults: a store that overrides only the required methods ───────────

#[derive(Default)]
struct DefaultsOnlyStore {
    inner: MemorySessionStore,
}

#[async_trait]
impl SessionStore for DefaultsOnlyStore {
    async fn append(&self, t: &str, s: &str, e: &SessionEvent) -> Result<u64> {
        self.inner.append(t, s, e).await
    }
    async fn read(&self, t: &str, s: &str) -> Result<Vec<StoredEvent>> {
        self.inner.read(t, s).await
    }
    async fn read_after(&self, t: &str, s: &str, after: u64) -> Result<Vec<StoredEvent>> {
        self.inner.read_after(t, s, after).await
    }
    async fn list_sessions(&self, t: &str) -> Result<Vec<SessionMeta>> {
        self.inner.list_sessions(t).await
    }
    async fn session_meta(&self, t: &str, s: &str) -> Result<Option<SessionMeta>> {
        self.inner.session_meta(t, s).await
    }
    async fn set_label(&self, t: &str, s: &str, label: Option<&str>) -> Result<()> {
        self.inner.set_label(t, s, label).await
    }
    async fn delete_session(&self, t: &str, s: &str) -> Result<()> {
        self.inner.delete_session(t, s).await
    }
    // Everything else uses the trait defaults.
}

fn msg(text: &str) -> SessionEvent {
    SessionEvent::Message {
        run_id: "r".into(),
        msg: Message::user(text),
        at: Utc::now(),
    }
}

#[tokio::test]
async fn default_append_batch_loops_in_order() {
    let s = DefaultsOnlyStore::default();
    s.append_batch("t", "s", &[msg("a"), msg("b"), msg("c")])
        .await
        .unwrap();
    let seqs: Vec<u64> = s
        .read("t", "s")
        .await
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(seqs, vec![1, 2, 3]);
}

#[tokio::test]
async fn default_read_after_limited_truncates() {
    let s = DefaultsOnlyStore::default();
    for i in 0..10 {
        s.append("t", "s", &msg(&format!("m{i}"))).await.unwrap();
    }
    let page = s.read_after_limited("t", "s", 2, 3).await.unwrap();
    assert_eq!(
        page.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![3, 4, 5]
    );
}

#[tokio::test]
async fn default_read_run_after_filters_by_run() {
    let s = DefaultsOnlyStore::default();
    s.append(
        "t",
        "s",
        &SessionEvent::Message {
            run_id: "r1".into(),
            msg: Message::user("x"),
            at: Utc::now(),
        },
    )
    .await
    .unwrap();
    s.append(
        "t",
        "s",
        &SessionEvent::Message {
            run_id: "r2".into(),
            msg: Message::user("y"),
            at: Utc::now(),
        },
    )
    .await
    .unwrap();
    let r2 = s.read_run_after("t", "s", "r2", 0).await.unwrap();
    assert_eq!(r2.len(), 1);
    assert_eq!(r2[0].event.run_id(), "r2");
}

#[tokio::test]
async fn default_list_sessions_page_keyset_filters() {
    let s = DefaultsOnlyStore::default();
    for _ in 0..5 {
        s.append(
            "t",
            &format!("s{}", uuid::Uuid::new_v4().simple()),
            &msg("x"),
        )
        .await
        .unwrap();
    }
    let first = s.list_sessions_page("t", None, 2).await.unwrap();
    assert_eq!(first.len(), 2);
    let last = first.last().unwrap();
    let next = s
        .list_sessions_page("t", Some((last.last_activity, last.session_id.clone())), 2)
        .await
        .unwrap();
    // the cursor advances — the next page starts strictly after the previous
    assert!(next.iter().all(|m| m.session_id != first[0].session_id));
}

#[tokio::test]
async fn default_optional_ops_report_unsupported() {
    let s = DefaultsOnlyStore::default();
    assert!(matches!(
        s.search("t", "q", 10, None).await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        s.cleanup_stale(chrono::Duration::seconds(1)).await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(s.list_tenants().await, Err(Error::Unsupported(_))));
}
