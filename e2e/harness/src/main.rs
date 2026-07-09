use std::time::Duration;

use runic_e2e_harness::dummy_agents;
use runic_serve::{RedisBroker, ServeConfig, WorkerConfig, serve};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,runic_serve=info,runic_agent=info".into()),
        )
        .init();

    let database_url =
        std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("DATABASE_URL is required"))?;
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8920);
    let instance = std::env::var("INSTANCE_ID").unwrap_or_else(|_| "harness".into());
    let redis_url = std::env::var("REDIS_URL").ok();
    let real_mistral = std::env::var("RUNIC_REAL_MISTRAL").is_ok();

    let sessions = runic_substrate::sessions_postgres(&database_url).await?;
    let blobs = runic_substrate::blobs_postgres_or_local(&database_url, "/tmp/runic-blobs").await;

    let mut config = ServeConfig::new(sessions.store(), blobs.store(), dummy_agents(real_mistral))
        .workers(WorkerConfig {
            max_concurrent_runs: 8,
            poll_every: Duration::from_millis(500),
        });

    let use_redis = if let Some(url) = &redis_url {
        let broker = RedisBroker::connect(url).await?;
        config = config.broker(broker.clone()).nudge(broker);
        true
    } else {
        false
    };

    let addr = format!("0.0.0.0:{port}");
    tracing::info!(%instance, %addr, redis = use_redis, real_mistral, "harness up");
    serve(config, addr).await?;
    Ok(())
}
