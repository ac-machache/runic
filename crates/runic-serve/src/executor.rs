use std::sync::Arc;
use std::time::Duration;

use runic_substrate::{RunRecord, RunStatus, SessionStore};
use runic_types::Message;
use tokio::sync::{Semaphore, mpsc};

use crate::factory::BoxedAgentFactory;
use crate::registry::{AgentRegistry, RunRegistry, as_chrono, spawn_heartbeat};
use crate::routes::runs::flush_persist;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub max_concurrent_runs: usize,
    pub poll_every: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_runs: 16,
            poll_every: Duration::from_millis(500),
        }
    }
}

pub fn spawn_run_workers(
    store: Arc<dyn SessionStore>,
    agents: Arc<AgentRegistry>,
    registry: Arc<RunRegistry>,
    config: WorkerConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_runs.max(1)));
        let lease = as_chrono(registry.limits().run_lease);
        tracing::info!(
            instance = registry.instance_id(),
            max_concurrent = config.max_concurrent_runs,
            poll_ms = config.poll_every.as_millis() as u64,
            "run workers started"
        );
        loop {
            let Ok(permit) = semaphore.clone().acquire_owned().await else {
                break;
            };
            match store
                .claim_next_queued_run(registry.instance_id(), lease)
                .await
            {
                Ok(Some(record)) => {
                    let (store, agents, registry) =
                        (store.clone(), agents.clone(), registry.clone());
                    tokio::spawn(async move {
                        execute_queued_run(store, agents, registry, record).await;
                        drop(permit);
                    });
                }
                Ok(None) => {
                    drop(permit);
                    tokio::time::sleep(config.poll_every).await;
                }
                Err(runic_substrate::Error::Unsupported(_)) => {
                    tracing::warn!("store has no run rows — run workers off");
                    break;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "queued-run dequeue failed");
                    drop(permit);
                    tokio::time::sleep(config.poll_every).await;
                }
            }
        }
    })
}

async fn execute_queued_run(
    store: Arc<dyn SessionStore>,
    agents: Arc<AgentRegistry>,
    registry: Arc<RunRegistry>,
    record: RunRecord,
) {
    let tenant = record.tenant.clone();
    let thread_id = record.session_id.clone();
    let run_id = record.run_id.clone();

    let factory: BoxedAgentFactory = match agents.factory(&record.agent) {
        Ok(f) => f.clone(),
        Err(_) => {
            fail(&store, &run_id, "queued run names an unknown agent").await;
            return;
        }
    };
    let user_msg = record
        .input
        .clone()
        .and_then(|v| serde_json::from_value::<Message>(v).ok());
    let Some(user_msg) = user_msg else {
        fail(&store, &run_id, "queued run has no stored input").await;
        return;
    };
    let ctx_json = record.context.clone().unwrap_or(serde_json::Value::Null);
    let mut run_ctx = factory
        .build_run_context(&tenant, &thread_id, &ctx_json)
        .await;

    let mut begun = match registry.begin(&tenant, &thread_id, &run_id).await {
        Ok(b) => b,
        Err(_) => {
            let _ = store.release_run(&run_id, registry.instance_id()).await;
            return;
        }
    };
    let steering_rx = std::mem::replace(&mut begun.steering_rx, mpsc::unbounded_channel().1);
    run_ctx = run_ctx
        .with_cancel(begun.cancel.clone())
        .with_steering(steering_rx)
        .with_agent(&record.agent)
        .with_run_id(&run_id);

    let heartbeat = spawn_heartbeat(
        store.clone(),
        run_id.clone(),
        registry.instance_id().to_string(),
        as_chrono(registry.limits().run_lease),
        registry.limits().heartbeat_every,
        begun.cancel.clone(),
    );

    tracing::info!(%tenant, %thread_id, %run_id, agent = %record.agent, "worker picked up run");
    let lock = registry.thread_lock(&tenant, &thread_id).await;
    let _guard = lock.lock().await;
    let mut agent =
        crate::registry::hydrate_agent(&store, &factory, &tenant, &thread_id, &mut begun).await;
    let outcome = agent.run_message_with(user_msg, run_ctx).await;
    heartbeat.abort();
    let (status, error) = match &outcome {
        Ok(o) if o.stop_reason.as_deref() == Some("cancelled") => (RunStatus::Cancelled, None),
        Ok(_) => (RunStatus::Success, None),
        Err(e) => (RunStatus::Error, Some(e.to_string())),
    };
    if let Err(e) = store
        .set_run_status(&run_id, status, error.as_deref())
        .await
    {
        tracing::warn!(%tenant, %thread_id, %run_id, error = %e, "run row update failed");
    }
    if let Err(e) = &outcome {
        tracing::error!(%tenant, %thread_id, %run_id, error = %e, "queued run failed");
    }
    flush_persist(&begun.persist).await;
    registry
        .end(&tenant, &thread_id, &run_id, begun.persist.clone())
        .await;
}

async fn fail(store: &Arc<dyn SessionStore>, run_id: &str, reason: &str) {
    tracing::error!(%run_id, reason, "queued run rejected");
    if let Err(e) = store
        .set_run_status(run_id, RunStatus::Error, Some(reason))
        .await
    {
        tracing::warn!(%run_id, error = %e, "run row update failed");
    }
}
