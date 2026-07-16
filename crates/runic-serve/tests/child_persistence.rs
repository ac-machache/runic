use std::sync::Arc;

use async_trait::async_trait;
use runic_serve::child::child_persistence;
use runic_state::SessionEvent;
use runic_substrate::{Error, MemorySessionStore, Result, SessionMeta, SessionStore, StoredEvent};

struct BrokenStore {
    fail_create: bool,
}

#[async_trait]
impl SessionStore for BrokenStore {
    async fn append(&self, _: &str, _: &str, _: &SessionEvent) -> Result<u64> {
        Err(Error::Backend("db down".into()))
    }
    async fn append_batch(&self, _: &str, _: &str, _: &[SessionEvent]) -> Result<()> {
        Err(Error::Backend("db down".into()))
    }
    async fn append_batch_strict(&self, _: &str, _: &str, _: &[SessionEvent]) -> Result<()> {
        Err(Error::Backend("db down".into()))
    }
    async fn read(&self, _: &str, _: &str) -> Result<Vec<StoredEvent>> {
        Ok(Vec::new())
    }
    async fn read_after(&self, _: &str, _: &str, _: u64) -> Result<Vec<StoredEvent>> {
        Ok(Vec::new())
    }
    async fn list_sessions(&self, _: &str) -> Result<Vec<SessionMeta>> {
        Ok(Vec::new())
    }
    async fn session_meta(&self, _: &str, _: &str) -> Result<Option<SessionMeta>> {
        Ok(None)
    }
    async fn set_label(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    async fn delete_session(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn create_child_session(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
        if self.fail_create {
            Err(Error::Backend("db down".into()))
        } else {
            Ok(())
        }
    }
}

fn run_start(run: &str) -> SessionEvent {
    SessionEvent::RunStart {
        run_id: run.into(),
        agent: None,
        audit: None,
        at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn a_begun_child_persists_its_events_and_flushes_clean() {
    let store = Arc::new(MemorySessionStore::new());
    store
        .append("alice", "parent-1", &run_start("r0"))
        .await
        .unwrap();
    let handle = child_persistence(store.clone(), "alice", "parent-1");

    let sink = handle.0.begin("scout").await.unwrap();
    let child_id = sink.session_id().to_string();
    assert!(child_id.starts_with("chd-"));

    sink.sink().send(Arc::new(run_start("r1")));
    sink.flush().await.unwrap();

    let events = store.read("alice", &child_id).await.unwrap();
    assert_eq!(events.len(), 1);
    let meta = store
        .session_meta("alice", &child_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(meta.agent.as_deref(), Some("scout"));
    assert_eq!(meta.parent_session.as_deref(), Some("parent-1"));

    let nested = sink.nested();
    let grandchild = nested.0.begin("scribe").await.unwrap();
    let meta = store
        .session_meta("alice", grandchild.session_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(meta.parent_session.as_deref(), Some(child_id.as_str()));
}

#[tokio::test]
async fn begin_fails_when_the_store_cannot_create_the_child_row() {
    let store = Arc::new(BrokenStore { fail_create: true });
    let handle = child_persistence(store, "alice", "parent-1");
    let Err(err) = handle.0.begin("scout").await else {
        panic!("begin must fail when the child row can't be created");
    };
    assert!(err.to_string().contains("db down"));
}

#[tokio::test]
async fn flush_reports_failure_after_bounded_retries() {
    let store = Arc::new(BrokenStore { fail_create: false });
    let handle = child_persistence(store, "alice", "parent-1");

    let sink = handle.0.begin("scout").await.unwrap();
    sink.sink().send(Arc::new(run_start("r1")));

    let err = sink.flush().await.unwrap_err();
    assert!(
        err.to_string().contains("db down"),
        "flush surfaces the store error, got: {err}"
    );
}

struct HangingStore;

#[async_trait]
impl SessionStore for HangingStore {
    async fn append(&self, _: &str, _: &str, _: &SessionEvent) -> Result<u64> {
        std::future::pending().await
    }
    async fn append_batch(&self, _: &str, _: &str, _: &[SessionEvent]) -> Result<()> {
        std::future::pending().await
    }
    async fn append_batch_strict(&self, _: &str, _: &str, _: &[SessionEvent]) -> Result<()> {
        std::future::pending().await
    }
    async fn read(&self, _: &str, _: &str) -> Result<Vec<StoredEvent>> {
        Ok(Vec::new())
    }
    async fn read_after(&self, _: &str, _: &str, _: u64) -> Result<Vec<StoredEvent>> {
        Ok(Vec::new())
    }
    async fn list_sessions(&self, _: &str) -> Result<Vec<SessionMeta>> {
        Ok(Vec::new())
    }
    async fn session_meta(&self, _: &str, _: &str) -> Result<Option<SessionMeta>> {
        Ok(None)
    }
    async fn set_label(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    async fn delete_session(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn create_child_session(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn flush_is_deadline_bounded_when_the_store_hangs() {
    let handle = child_persistence(Arc::new(HangingStore), "alice", "parent-1");
    let sink = handle.0.begin("scout").await.unwrap();
    sink.sink().send(Arc::new(run_start("r1")));

    let err = sink.flush().await.unwrap_err();
    assert!(
        err.to_string().contains("timed out"),
        "a hung store cannot hang the delegation forever: {err}"
    );
    assert!(err.to_string().contains("1 events unflushed"));
}

struct SlowStore {
    batches: std::sync::Mutex<Vec<usize>>,
}

#[async_trait]
impl SessionStore for SlowStore {
    async fn append(&self, _: &str, _: &str, _: &SessionEvent) -> Result<u64> {
        Ok(0)
    }
    async fn append_batch(&self, _: &str, _: &str, batch: &[SessionEvent]) -> Result<()> {
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        self.batches.lock().unwrap().push(batch.len());
        Ok(())
    }
    async fn append_batch_strict(&self, t: &str, s: &str, batch: &[SessionEvent]) -> Result<()> {
        self.append_batch(t, s, batch).await
    }
    async fn read(&self, _: &str, _: &str) -> Result<Vec<StoredEvent>> {
        Ok(Vec::new())
    }
    async fn read_after(&self, _: &str, _: &str, _: u64) -> Result<Vec<StoredEvent>> {
        Ok(Vec::new())
    }
    async fn list_sessions(&self, _: &str) -> Result<Vec<SessionMeta>> {
        Ok(Vec::new())
    }
    async fn session_meta(&self, _: &str, _: &str) -> Result<Option<SessionMeta>> {
        Ok(None)
    }
    async fn set_label(&self, _: &str, _: &str, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    async fn delete_session(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    async fn create_child_session(&self, _: &str, _: &str, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn flush_timeout_stops_the_writer() {
    let store = Arc::new(SlowStore {
        batches: std::sync::Mutex::new(Vec::new()),
    });
    let handle = child_persistence(store.clone(), "alice", "parent-1");
    let sink = handle.0.begin("scout").await.unwrap();
    sink.sink().send(Arc::new(run_start("r1")));

    let err = sink.flush().await.unwrap_err();
    assert!(err.to_string().contains("timed out"));

    sink.sink().send(Arc::new(run_start("r2")));
    tokio::time::sleep(std::time::Duration::from_secs(120)).await;
    assert_eq!(
        store.batches.lock().unwrap().len(),
        1,
        "the in-flight batch may still land; nothing enqueued after the timeout does"
    );

    let second = sink.flush().await.unwrap_err();
    assert!(
        second.to_string().contains("timed out"),
        "a timed-out sink stays failed: {second}"
    );
}

#[tokio::test]
async fn a_crashed_child_stays_visibly_in_flight() {
    let store = Arc::new(MemorySessionStore::new());
    store
        .append("alice", "parent-1", &run_start("r0"))
        .await
        .unwrap();
    let handle = child_persistence(store.clone(), "alice", "parent-1");

    let sink = handle.0.begin("scout").await.unwrap();
    let child_id = sink.session_id().to_string();
    sink.sink().send(Arc::new(run_start("r1")));
    sink.flush().await.unwrap();
    drop(sink);

    let events = store.read("alice", &child_id).await.unwrap();
    let timeline = runic_state::timeline::project(events.iter().map(|stored| &stored.event));
    assert_eq!(timeline.len(), 1);
    assert_eq!(
        timeline[0].status,
        runic_state::TraceStatus::InFlight,
        "a child with no RunEnd is detectably incomplete"
    );

    let children = store
        .list_sessions_page(
            "alice",
            None,
            10,
            runic_substrate::SessionScope::ChildrenOf("parent-1".into()),
        )
        .await
        .unwrap();
    assert!(
        children.iter().any(|m| m.session_id == child_id),
        "the crashed child is still reachable through its parent"
    );
}

#[tokio::test]
async fn a_deleted_child_is_never_resurrected_by_late_appends() {
    let store = Arc::new(MemorySessionStore::new());
    store
        .append("alice", "parent-1", &run_start("r0"))
        .await
        .unwrap();
    let handle = child_persistence(store.clone(), "alice", "parent-1");

    let sink = handle.0.begin("scout").await.unwrap();
    let child_id = sink.session_id().to_string();
    sink.sink().send(Arc::new(run_start("r1")));
    sink.flush().await.unwrap();

    store.delete_session("alice", &child_id).await.unwrap();

    sink.sink().send(Arc::new(run_start("r2")));
    let err = sink.flush().await.unwrap_err();
    assert!(
        err.to_string().contains("not found"),
        "the late batch surfaces the deletion instead of resurrecting: {err}"
    );

    assert!(
        store
            .session_meta("alice", &child_id)
            .await
            .unwrap()
            .is_none(),
        "the deleted child stays deleted"
    );
    let roots = store
        .list_sessions_page("alice", None, 10, runic_substrate::SessionScope::Roots)
        .await
        .unwrap();
    assert!(
        roots.iter().all(|m| m.session_id != child_id),
        "no ghost root thread appears"
    );
}
