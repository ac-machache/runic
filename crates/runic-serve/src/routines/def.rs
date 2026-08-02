use super::context::RoutineContext;
use async_trait::async_trait;

#[async_trait]
pub trait Routine: Send + Sync + 'static {
    async fn run(&self, ctx: &RoutineContext) -> anyhow::Result<()>;
}
