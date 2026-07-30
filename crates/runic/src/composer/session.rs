use std::sync::Arc;

use runic_state::ThreadStats;
use runic_substrate::{
    Blobs, SessionMeta, SessionScope, SessionStore, Sessions, StoreSubSession, StoredEvent,
    attach_persister, replay_messages,
};

use runic_substrate::Result as StoreResult;

const CHILD_PAGE: usize = 500;
use runic_types::Message;
use tracing::Instrument;

use super::{Agent, AgentOutput};
use crate::Input;

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

    fn require_store(&self) -> StoreResult<Arc<dyn SessionStore>> {
        match &self.sessions {
            Some(sessions) => Ok(sessions.store()),
            None => Err(runic_substrate::Error::Unsupported(
                "this session has no store: call .store(sessions)".into(),
            )),
        }
    }

    pub async fn meta(&self) -> StoreResult<Option<SessionMeta>> {
        let Some(sessions) = &self.sessions else {
            return Ok(None);
        };
        sessions
            .store()
            .session_meta(self.tenant(), self.thread())
            .await
    }

    pub async fn label(&self) -> StoreResult<Option<String>> {
        Ok(self.meta().await?.and_then(|meta| meta.label))
    }

    pub async fn set_label(&self, label: Option<&str>) -> StoreResult<()> {
        self.require_store()?
            .set_label(self.tenant(), self.thread(), label)
            .await?;
        Ok(())
    }

    pub async fn events(&self) -> StoreResult<Vec<StoredEvent>> {
        let Some(sessions) = &self.sessions else {
            return Ok(Vec::new());
        };
        sessions.store().read(self.tenant(), self.thread()).await
    }

    /// A page of the log after `after_seq`, plus whether more remain.
    pub async fn events_after(
        &self,
        after_seq: u64,
        limit: usize,
    ) -> StoreResult<(Vec<StoredEvent>, bool)> {
        let Some(sessions) = &self.sessions else {
            return Ok((Vec::new(), false));
        };
        let mut page = sessions
            .store()
            .read_after_limited(self.tenant(), self.thread(), after_seq, limit + 1)
            .await?;
        let has_more = page.len() > limit;
        page.truncate(limit);
        Ok((page, has_more))
    }

    /// Folded from the tail: a `StateSnapshot` carries the rolled-up totals and
    /// replaces rather than accumulates, so reading past one changes nothing.
    pub async fn stats(&self) -> StoreResult<ThreadStats> {
        let Some(sessions) = &self.sessions else {
            return Ok(ThreadStats::default());
        };
        let stored = sessions
            .store()
            .read_tail(self.tenant(), self.thread())
            .await?;
        let mut stats = ThreadStats::default();
        for entry in stored {
            stats.fold(&entry.event.lift());
        }
        Ok(stats)
    }

    /// This thread and every thread delegated from it, parents before children.
    /// Reverse it to delete: a child outliving its parent is an orphan, the
    /// other way round is just a partial delete.
    pub async fn descendants(&self) -> StoreResult<Vec<String>> {
        let store = self.require_store()?;
        let mut pending = vec![self.thread().to_string()];
        let mut order: Vec<String> = Vec::new();
        while let Some(thread) = pending.pop() {
            let mut cursor = None;
            loop {
                let page = store
                    .list_sessions_page(
                        self.tenant(),
                        cursor,
                        CHILD_PAGE,
                        SessionScope::ChildrenOf(thread.clone()),
                    )
                    .await?;
                let Some(last) = page.last() else { break };
                cursor = Some((last.last_activity, last.session_id.clone()));
                let exhausted = page.len() < CHILD_PAGE;
                pending.extend(page.into_iter().map(|meta| meta.session_id));
                if exhausted {
                    break;
                }
            }
            order.push(thread);
        }
        Ok(order)
    }

    pub async fn delete_tree(&self) -> StoreResult<usize> {
        let store = self.require_store()?;
        let order = self.descendants().await?;
        for thread in order.iter().rev() {
            store.delete_session(self.tenant(), thread).await?;
        }
        Ok(order.len())
    }

    pub async fn messages(&self) -> StoreResult<Vec<Message>> {
        let Some(sessions) = &self.sessions else {
            return Ok(Vec::new());
        };
        replay_messages(sessions.store().as_ref(), self.tenant(), self.thread()).await
    }

    pub async fn delete(&self) -> StoreResult<()> {
        self.require_store()?
            .delete_session(self.tenant(), self.thread())
            .await?;
        Ok(())
    }

    pub async fn invoke(&self, agent: &Agent, input: Input) -> anyhow::Result<AgentOutput> {
        let (message, mut ctx) = input.split();
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
            );
            ctx.events.push(emitter);
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
