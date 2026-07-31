use std::time::Duration;

use apalis_postgres::{CompactType, Config, JsonCodec, PgNotify, PgPool, PostgresStorage};
use runic_types::Message;
use serde::{Deserialize, Serialize};

pub const QUEUE: &str = "runic::run";
pub const MAX_ATTEMPTS: u32 = 3;
pub const TURN_POLL: Duration = Duration::from_millis(200);
pub const TURN_TRIES: u32 = 10;
pub const DEFER_AFTER: Duration = Duration::from_secs(5);
pub const MAX_WAVES: u32 = 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunJob {
    pub tenant: String,
    pub thread_id: String,
    pub run_id: String,
    pub agent: String,
    pub message: Message,
    pub context: Option<serde_json::Value>,
    #[serde(default)]
    pub wave: u32,
}

impl RunJob {
    pub fn key(&self) -> String {
        format!("{}:{}", self.run_id, self.wave)
    }
}

pub type RunSink = PostgresStorage<RunJob>;
pub type RunListener = PostgresStorage<RunJob, CompactType, JsonCodec<CompactType>, PgNotify>;

pub fn config() -> Config {
    Config::new(QUEUE)
}

pub fn sink(pool: &PgPool) -> RunSink {
    PostgresStorage::new_with_config(pool, &config())
}

pub fn listener(pool: &PgPool) -> RunListener {
    PostgresStorage::new_with_notify(pool, &config())
}

pub async fn setup(pool: &PgPool) -> anyhow::Result<()> {
    PostgresStorage::<(), (), ()>::setup(pool).await?;
    Ok(())
}
