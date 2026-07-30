use std::sync::Arc;

use runic_agent::RunContext;
use runic_substrate::{
    Blobs, SessionMeta, SessionStore, Sessions, StoreSubSession, StoredEvent, attach_persister,
    replay_messages,
};
use runic_types::Message;
use tracing::Instrument;

use super::{Agent, AgentOutput};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    tenant: String,
    id: String,
}

impl Thread {
    pub fn new(tenant: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            id: id.into(),
        }
    }

    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl<T: Into<String>, I: Into<String>> From<(T, I)> for Thread {
    fn from((tenant, id): (T, I)) -> Self {
        Thread::new(tenant, id)
    }
}

pub struct Session {
    thread: Thread,
    sessions: Option<Sessions>,
    blobs: Option<Blobs>,
}

pub fn session(thread: impl Into<Thread>) -> Session {
    Session::new(thread)
}

impl Session {
    pub fn new(thread: impl Into<Thread>) -> Self {
        Self {
            thread: thread.into(),
            sessions: None,
            blobs: None,
        }
    }

    pub fn store(mut self, sessions: impl Into<Sessions>) -> Self {
        self.sessions = Some(sessions.into());
        self
    }

    pub fn artifacts(mut self, blobs: impl Into<Blobs>) -> Self {
        self.blobs = Some(blobs.into());
        self
    }

    pub fn tenant(&self) -> &str {
        self.thread.tenant()
    }

    pub fn thread(&self) -> &str {
        self.thread.id()
    }

    fn require_store(&self) -> anyhow::Result<Arc<dyn SessionStore>> {
        match &self.sessions {
            Some(sessions) => Ok(sessions.store()),
            None => anyhow::bail!("this session has no store: call .store(sessions)"),
        }
    }

    pub async fn meta(&self) -> anyhow::Result<Option<SessionMeta>> {
        let Some(sessions) = &self.sessions else {
            return Ok(None);
        };
        Ok(sessions
            .store()
            .session_meta(self.tenant(), self.thread())
            .await?)
    }

    pub async fn label(&self) -> anyhow::Result<Option<String>> {
        Ok(self.meta().await?.and_then(|meta| meta.label))
    }

    pub async fn set_label(&self, label: Option<&str>) -> anyhow::Result<()> {
        self.require_store()?
            .set_label(self.tenant(), self.thread(), label)
            .await?;
        Ok(())
    }

    pub async fn events(&self) -> anyhow::Result<Vec<StoredEvent>> {
        let Some(sessions) = &self.sessions else {
            return Ok(Vec::new());
        };
        Ok(sessions.store().read(self.tenant(), self.thread()).await?)
    }

    pub async fn messages(&self) -> anyhow::Result<Vec<Message>> {
        let Some(sessions) = &self.sessions else {
            return Ok(Vec::new());
        };
        Ok(replay_messages(sessions.store().as_ref(), self.tenant(), self.thread()).await?)
    }

    pub async fn delete(&self) -> anyhow::Result<()> {
        self.require_store()?
            .delete_session(self.tenant(), self.thread())
            .await?;
        Ok(())
    }

    pub async fn run(
        &self,
        agent: &Agent,
        message: impl Into<String>,
    ) -> anyhow::Result<AgentOutput> {
        self.run_message(agent, Message::user(message.into())).await
    }

    pub async fn run_message(
        &self,
        agent: &Agent,
        message: Message,
    ) -> anyhow::Result<AgentOutput> {
        self.run_message_with(agent, message, RunContext::new())
            .await
    }

    pub async fn run_message_with(
        &self,
        agent: &Agent,
        message: Message,
        mut ctx: RunContext,
    ) -> anyhow::Result<AgentOutput> {
        let span = tracing::info_span!(
            "session_run",
            tenant = %self.tenant(),
            thread = %self.thread(),
            persisted = self.sessions.is_some(),
            persist_backlog_at_flush = tracing::field::Empty,
            flush_ms = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        let result = async {
            let hydrate_span = tracing::info_span!(
                "hydrate",
                tenant = %self.tenant(),
                thread = %self.thread(),
                events = tracing::field::Empty,
            );
            let mut runner = async {
                let mut bound = agent.clone();
                if let Some(blobs) = &self.blobs {
                    bound = bound.tools(blobs.tools().iter().cloned());
                    bound = bound.hooks(blobs.hooks().iter().cloned());
                }
                if let Some(sessions) = &self.sessions {
                    bound = bound.tools(sessions.tools().iter().cloned());
                    bound = bound.hooks(sessions.hooks().iter().cloned());
                }
                let mut runner = bound.build(self.tenant(), self.thread()).await?;
                if let Some(sessions) = &self.sessions {
                    let entries = sessions
                        .store()
                        .read_tail(self.tenant(), self.thread())
                        .await?;
                    tracing::Span::current().record("events", entries.len());
                    for entry in entries {
                        runner.state_mut().fold(&entry.event.lift());
                    }
                }
                Ok::<_, anyhow::Error>(runner)
            }
            .instrument(hydrate_span)
            .await?;

            let Some(sessions) = &self.sessions else {
                let outcome = runner
                    .run_message_with(message, ctx)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                return Ok(AgentOutput::from_run(&runner, outcome));
            };

            let (emitter, handle) = attach_persister(
                sessions.store(),
                self.tenant().to_string(),
                self.thread().to_string(),
                ctx.events.take(),
            );
            ctx.events = Some(emitter);
            if ctx.sub_session.is_none() {
                ctx.sub_session = Some(Arc::new(StoreSubSession::new(
                    sessions.store(),
                    self.tenant().to_string(),
                    self.thread().to_string(),
                )));
            }

            let outcome = runner
                .run_message_with(message, ctx)
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
