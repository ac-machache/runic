use async_trait::async_trait;

#[async_trait]
pub trait SkillSource: Send + Sync {
    async fn entries(&self) -> anyhow::Result<Vec<String>>;

    async fn read(&self, rel: &str) -> anyhow::Result<String>;
}
