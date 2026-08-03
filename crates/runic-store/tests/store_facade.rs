use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use runic_hook::{HookOutcome, WriteHook};
use runic_state::AgentState;
use runic_store::artifacts::{self, ArtifactSource};
use runic_store::{ArtifactStore, SessionEvent, SessionStore, Store};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::Message;

fn msg(text: &str) -> SessionEvent {
    SessionEvent::Message {
        run_id: "r".into(),
        msg: Message::user(text),
        at: chrono::Utc::now(),
    }
}

struct Inventory {
    sessions: Arc<dyn SessionStore>,
    artifacts: Arc<dyn ArtifactStore>,
}

#[async_trait]
impl Tool for Inventory {
    fn name(&self) -> &str {
        "inventory"
    }

    fn description(&self) -> &str {
        "count a session's events and artifacts"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let events = self
            .sessions
            .read(&ctx.user_id, &ctx.session_id)
            .await?
            .len();
        let artifacts = self
            .artifacts
            .list(&ctx.user_id, &ctx.session_id)
            .await?
            .len();
        Ok(ToolResult::ok(format!(
            "{events} events, {artifacts} artifacts"
        )))
    }
}

#[tokio::test]
async fn a_store_tool_can_reach_the_log_and_the_artifacts() {
    let store = Store::memory().unwrap().tool_with(|store| Inventory {
        sessions: store.sessions(),
        artifacts: store.artifacts(),
    });

    store
        .sessions()
        .append("t", "s", &msg("one"))
        .await
        .unwrap();
    store
        .sessions()
        .append("t", "s", &msg("two"))
        .await
        .unwrap();
    store
        .artifacts()
        .put("t", "s", "text/plain", ArtifactSource::UserUpload, b"blob")
        .await
        .unwrap();

    let tools = store.tools();
    assert_eq!(tools.len(), 1);
    let ctx = ToolContext::new("t", "s", "r");
    let out = tools[0].execute(serde_json::json!({}), &ctx).await.unwrap();
    assert_eq!(out.text(), "2 events, 1 artifacts");
}

#[tokio::test]
async fn registration_order_does_not_strand_a_tool_on_the_old_backend() {
    let elsewhere = tempfile::tempdir().unwrap();

    let store = Store::memory()
        .unwrap()
        .tool_with(|store| Inventory {
            sessions: store.sessions(),
            artifacts: store.artifacts(),
        })
        .artifacts_location(artifacts::local(elsewhere.path().to_string_lossy()).unwrap());

    store
        .sessions()
        .append("t", "s", &msg("one"))
        .await
        .unwrap();
    store
        .artifacts()
        .put("t", "s", "text/plain", ArtifactSource::UserUpload, b"blob")
        .await
        .unwrap();

    let ctx = ToolContext::new("t", "s", "r");
    let out = store.tools()[0]
        .execute(serde_json::json!({}), &ctx)
        .await
        .unwrap();
    assert_eq!(
        out.text(),
        "1 events, 1 artifacts",
        "the tool followed the relocated artifact store"
    );
    assert!(
        elsewhere.path().join("blobs").exists(),
        "bytes really did move to the new location"
    );
}

#[derive(Clone)]
struct CountingHook(Arc<AtomicUsize>);

#[async_trait]
impl WriteHook for CountingHook {
    fn name(&self) -> &str {
        "counting"
    }

    async fn before_agent(&self, _state: &mut AgentState) -> HookOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        HookOutcome::Noop
    }
}

#[tokio::test]
async fn plain_tools_and_hooks_still_register_without_a_store_handle() {
    let seen = Arc::new(AtomicUsize::new(0));
    let store = Store::memory().unwrap().hook(CountingHook(seen.clone()));

    assert_eq!(store.hooks().len(), 1);
    assert!(store.tools().is_empty());
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn a_local_store_puts_the_log_and_the_bytes_under_one_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::local(dir.path()).await.unwrap();

    store
        .sessions()
        .append("t", "s", &msg("durable"))
        .await
        .unwrap();
    store
        .artifacts()
        .put("t", "s", "text/plain", ArtifactSource::UserUpload, b"bytes")
        .await
        .unwrap();

    assert!(dir.path().join("runic.db").exists(), "sqlite log");
    assert!(dir.path().join("artifacts").join("blobs").exists(), "bytes");

    let reopened = Store::local(dir.path()).await.unwrap();
    assert_eq!(reopened.sessions().read("t", "s").await.unwrap().len(), 1);
    assert_eq!(reopened.artifacts().list("t", "s").await.unwrap().len(), 1);
}
