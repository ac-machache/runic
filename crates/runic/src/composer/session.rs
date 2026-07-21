use std::sync::Arc;

use runic_agent::RunContext;
use runic_substrate::{SessionStore, StoreSubSession, attach_persister};
use tracing::Instrument;

use super::{Agent, AgentOutput};

pub struct Session {
    agent: Agent,
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
}

impl Session {
    pub fn new(
        agent: Agent,
        store: Arc<dyn SessionStore>,
        tenant: String,
        session_id: String,
    ) -> Self {
        Self {
            agent,
            store,
            tenant,
            session_id,
        }
    }

    pub async fn run(&self, message: impl Into<String>) -> anyhow::Result<AgentOutput> {
        let message = message.into();
        let span = tracing::info_span!(
            "session_run",
            tenant = %self.tenant,
            thread = %self.session_id,
            persist_backlog_at_flush = tracing::field::Empty,
            flush_ms = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        let result = async {
            let hydrate_span = tracing::info_span!(
                "hydrate",
                tenant = %self.tenant,
                thread = %self.session_id,
                events = tracing::field::Empty,
            );
            let mut runner = async {
                let mut runner = self.agent.build(&self.tenant, &self.session_id).await?;
                let entries = self.store.read_tail(&self.tenant, &self.session_id).await?;
                tracing::Span::current().record("events", entries.len());
                for entry in entries {
                    runner.state_mut().fold(&entry.event.lift());
                }
                Ok::<_, anyhow::Error>(runner)
            }
            .instrument(hydrate_span)
            .await?;

            let (emitter, handle) = attach_persister(
                self.store.clone(),
                self.tenant.clone(),
                self.session_id.clone(),
            );
            let sub_session = Arc::new(StoreSubSession::new(
                self.store.clone(),
                self.tenant.clone(),
                self.session_id.clone(),
            ));
            let ctx = RunContext::new()
                .with_events(emitter)
                .with_sub_session(sub_session);

            let outcome = runner
                .run_with(message, ctx)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            let current = tracing::Span::current();
            current.record("persist_backlog_at_flush", handle.backlog());
            let started = std::time::Instant::now();
            handle.flush().await?;
            current.record("flush_ms", started.elapsed().as_millis() as u64);

            Ok(AgentOutput::from_run(&runner, outcome))
        }
        .instrument(span.clone())
        .await;

        if result.is_err() {
            span.record("otel.status_code", "ERROR");
        }
        result
    }
}
