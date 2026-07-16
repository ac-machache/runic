use std::sync::Arc;

use async_trait::async_trait;

use crate::state::PersistSink;

#[async_trait]
pub trait ChildPersistence: Send + Sync {
    async fn begin(&self, agent: &str) -> anyhow::Result<Box<dyn ChildSink>>;
}

#[async_trait]
pub trait ChildSink: Send + Sync {
    fn session_id(&self) -> &str;

    fn sink(&self) -> PersistSink;

    fn nested(&self) -> ChildPersistenceHandle;

    async fn flush(&self) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub struct ChildPersistenceHandle(pub Arc<dyn ChildPersistence>);
