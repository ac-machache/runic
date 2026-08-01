use std::sync::Arc;
use std::time::Duration;

use crate::store::{RunOutput, RunStatus};

pub const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct FinishedRun {
    pub tenant: String,
    pub run_id: String,
    pub session_id: Option<String>,
    pub agent: String,
    pub status: RunStatus,
    pub error: Option<String>,
    pub output: RunOutput,
}

impl FinishedRun {
    pub fn stateless(&self) -> bool {
        self.session_id.is_none()
    }
}

#[async_trait::async_trait]
pub trait RunHook: Send + Sync + 'static {
    async fn finished(&self, run: &FinishedRun) -> anyhow::Result<()>;
}

#[derive(Default, Clone)]
pub struct HookRegistry(std::collections::HashMap<String, Arc<dyn RunHook>>);

impl HookRegistry {
    pub fn insert(&mut self, name: impl Into<String>, hook: Arc<dyn RunHook>) {
        self.0.insert(name.into(), hook);
    }

    pub fn knows(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn RunHook>> {
        self.0.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

pub(crate) async fn fire(hook: &Arc<dyn RunHook>, name: &str, run: FinishedRun) {
    let run_id = run.run_id.clone();
    match tokio::time::timeout(HOOK_TIMEOUT, hook.finished(&run)).await {
        Ok(Ok(())) => tracing::info!(%run_id, hook = name, "run hook fired"),
        Ok(Err(error)) => tracing::error!(%run_id, hook = name, %error, "run hook failed"),
        Err(_) => tracing::error!(
            %run_id,
            hook = name,
            secs = HOOK_TIMEOUT.as_secs(),
            "run hook timed out"
        ),
    }
}
