use std::sync::Arc;

use async_trait::async_trait;

use crate::Emitter;

#[async_trait]
pub trait SubSession: Send + Sync {
    async fn begin(&self, agent: &str) -> anyhow::Result<Box<dyn SubRun>>;
}

#[async_trait]
pub trait SubRun: Send + Sync {
    fn session_id(&self) -> &str;
    fn emitter(&self) -> Arc<dyn Emitter>;
    fn nested(&self) -> Arc<dyn SubSession>;
    async fn flush(&self) -> anyhow::Result<()>;
}
