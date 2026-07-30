use std::sync::Arc;

use runic_agent::RunContext;
use runic_substrate::{Blobs, Sessions, StoreSubSession, attach_persister};
use tracing::Instrument;

use super::{Agent, AgentOutput};

pub struct Session {
    sessions: Sessions,
    tenant: String,
    session_id: String,
    blobs: Option<Blobs>,
}

pub fn session(
    sessions: impl Into<Sessions>,
    tenant: impl Into<String>,
    thread: impl Into<String>,
) -> Session {
    Session::new(sessions, tenant, thread)
}

impl Session {
    pub fn new(
        sessions: impl Into<Sessions>,
        tenant: impl Into<String>,
        thread: impl Into<String>,
    ) -> Self {
        Self {
            sessions: sessions.into(),
            tenant: tenant.into(),
            session_id: thread.into(),
            blobs: None,
        }
    }

    pub fn artifacts(mut self, blobs: impl Into<Blobs>) -> Self {
        self.blobs = Some(blobs.into());
        self
    }

    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    pub fn thread(&self) -> &str {
        &self.session_id
    }

    pub async fn run(
        &self,
        agent: &Agent,
        message: impl Into<String>,
    ) -> anyhow::Result<AgentOutput> {
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
                let mut bound = agent.clone();
                if let Some(blobs) = &self.blobs {
                    if blobs.tools().is_empty() {
                        tracing::warn!(
                            tenant = %self.tenant,
                            thread = %self.session_id,
                            "artifact store has no tool: uploads are written but the model \
                             cannot read them back — hand the store a reader tool"
                        );
                    }
                    bound = bound.artifacts(blobs.store());
                    bound = bound.tools(blobs.tools().iter().cloned());
                    bound = bound.hooks(blobs.hooks().iter().cloned());
                }
                bound = bound.tools(self.sessions.tools().iter().cloned());
                bound = bound.hooks(self.sessions.hooks().iter().cloned());
                let mut runner = bound.build(&self.tenant, &self.session_id).await?;
                let entries = self
                    .sessions
                    .store()
                    .read_tail(&self.tenant, &self.session_id)
                    .await?;
                tracing::Span::current().record("events", entries.len());
                for entry in entries {
                    runner.state_mut().fold(&entry.event.lift());
                }
                Ok::<_, anyhow::Error>(runner)
            }
            .instrument(hydrate_span)
            .await?;

            let (emitter, handle) = attach_persister(
                self.sessions.store(),
                self.tenant.clone(),
                self.session_id.clone(),
            );
            let sub_session = Arc::new(StoreSubSession::new(
                self.sessions.store(),
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
