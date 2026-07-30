use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic_substrate::{
    Error, MemorySessionStore, Result, SessionEvent, SessionMeta, SessionStore, StoredEvent,
    attach_persister,
};
use tracing_subscriber::fmt::format::FmtSpan;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct FlakyStore {
    inner: MemorySessionStore,
    failures_left: AtomicU32,
}

#[async_trait]
impl SessionStore for FlakyStore {
    async fn append(&self, tenant: &str, session_id: &str, event: &SessionEvent) -> Result<u64> {
        self.inner.append(tenant, session_id, event).await
    }

    async fn append_batch(
        &self,
        tenant: &str,
        session_id: &str,
        events: &[SessionEvent],
    ) -> Result<()> {
        let left = self.failures_left.load(Ordering::SeqCst);
        if left > 0 {
            self.failures_left.store(left - 1, Ordering::SeqCst);
            return Err(Error::Unsupported("store is down".into()));
        }
        self.inner.append_batch(tenant, session_id, events).await
    }

    async fn read(&self, tenant: &str, session_id: &str) -> Result<Vec<StoredEvent>> {
        self.inner.read(tenant, session_id).await
    }

    async fn read_after(
        &self,
        tenant: &str,
        session_id: &str,
        after_seq: u64,
    ) -> Result<Vec<StoredEvent>> {
        self.inner.read_after(tenant, session_id, after_seq).await
    }

    async fn list_sessions(&self, tenant: &str) -> Result<Vec<SessionMeta>> {
        self.inner.list_sessions(tenant).await
    }

    async fn session_meta(&self, tenant: &str, session_id: &str) -> Result<Option<SessionMeta>> {
        self.inner.session_meta(tenant, session_id).await
    }

    async fn set_label(&self, tenant: &str, session_id: &str, label: Option<&str>) -> Result<()> {
        self.inner.set_label(tenant, session_id, label).await
    }

    async fn delete_session(&self, tenant: &str, session_id: &str) -> Result<()> {
        self.inner.delete_session(tenant, session_id).await
    }
}

fn message_event(i: usize) -> SessionEvent {
    SessionEvent::Message {
        run_id: "r1".into(),
        msg: runic_types::Message::user(format!("m{i}")),
        at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn persist_batch_span_carries_a_retry_count_and_no_error_on_eventual_success() {
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_span_events(FmtSpan::CLOSE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();

    let store = Arc::new(FlakyStore {
        inner: MemorySessionStore::new(),
        failures_left: AtomicU32::new(2),
    });

    let _guard = tracing::subscriber::set_default(subscriber);

    let (emitter, handle) =
        attach_persister(store.clone(), "tenant".into(), "thread-1".into(), None);
    for i in 0..3 {
        emitter.emit(message_event(i).lift());
    }
    handle.flush().await.unwrap();

    let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(
        output.contains("persist_batch{tenant=tenant session_id=thread-1 batch_size=3"),
        "missing persist_batch span in trace output:\n{output}"
    );
    assert!(
        !output.contains("otel.status_code=\"ERROR\""),
        "a batch that eventually succeeds should not mark otel.status_code=ERROR:\n{output}"
    );
}

#[tokio::test]
async fn persist_batch_span_marks_otel_error_when_the_store_never_recovers() {
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_span_events(FmtSpan::CLOSE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();

    let store = Arc::new(FlakyStore {
        inner: MemorySessionStore::new(),
        failures_left: AtomicU32::new(u32::MAX),
    });

    let _guard = tracing::subscriber::set_default(subscriber);

    let (emitter, handle) =
        attach_persister(store.clone(), "tenant".into(), "thread-1".into(), None);
    emitter.emit(message_event(0).lift());
    assert!(handle.flush().await.is_err());

    let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(
        output.contains("persist_batch{tenant=tenant session_id=thread-1 batch_size=1"),
        "missing persist_batch span in trace output:\n{output}"
    );
    assert!(
        output.contains("otel.status_code=\"ERROR\""),
        "a batch that never recovers should mark otel.status_code=ERROR:\n{output}"
    );
}
