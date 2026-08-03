use std::sync::Arc;

use runic_agent::{AgentError, RunContext, Runner};
use runic_state::{AgentEvent, Deferral, PersistenceStatus, RunOutcome, SessionStats};
use runic_store::{
    SessionMeta, SessionScope, SessionStore, Store, StoreSubSession, StoredEvent, attach_persister,
    replay_messages,
};

use runic_store::Result as StoreResult;

const CHILD_PAGE: usize = 500;
use runic_store::artifacts::ArtifactSource;
use runic_types::{ContentBlock, Message, MessageContent, Source};
use tracing::Instrument;

use super::{Agent, AgentOutput};
use crate::Input;
use crate::builtin::ArtifactResolver;

fn attachment_mut(block: &mut ContentBlock) -> Option<(&str, &mut Source)> {
    match block {
        ContentBlock::Image {
            media_type, source, ..
        }
        | ContentBlock::File {
            media_type, source, ..
        } => Some((media_type, source)),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKey {
    tenant: String,
    id: String,
}

impl SessionKey {
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

async fn step(
    runner: &mut Runner,
    message: Option<Message>,
    ctx: RunContext,
) -> Result<RunOutcome, AgentError> {
    match message {
        Some(message) => runner.run_message_with(message, ctx).await,
        None => runner.resume(ctx).await,
    }
}

impl<T: Into<String>, I: Into<String>> From<(T, I)> for SessionKey {
    fn from((tenant, id): (T, I)) -> Self {
        SessionKey::new(tenant, id)
    }
}

pub struct Session {
    session: SessionKey,
    store: Option<Store>,
}

pub fn session(session: impl Into<SessionKey>) -> Session {
    Session::new(session)
}

impl Session {
    pub fn new(session: impl Into<SessionKey>) -> Self {
        Self {
            session: session.into(),
            store: None,
        }
    }

    /// Attach durable storage. Without it the session is stateless: no event
    /// log, no artifacts, no store-registered tools or hooks.
    pub fn store(mut self, store: impl Into<Store>) -> Self {
        self.store = Some(store.into());
        self
    }

    pub fn tenant(&self) -> &str {
        self.session.tenant()
    }

    pub fn session(&self) -> &str {
        self.session.id()
    }

    fn require_store(&self) -> StoreResult<Arc<dyn SessionStore>> {
        match &self.store {
            Some(store) => Ok(store.sessions()),
            None => Err(runic_store::Error::Unsupported(
                "this session has no store: call .store(store)".into(),
            )),
        }
    }

    pub async fn meta(&self) -> StoreResult<Option<SessionMeta>> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        store
            .sessions()
            .session_meta(self.tenant(), self.session())
            .await
    }

    pub async fn label(&self) -> StoreResult<Option<String>> {
        Ok(self.meta().await?.and_then(|meta| meta.label))
    }

    pub async fn set_label(&self, label: Option<&str>) -> StoreResult<()> {
        self.require_store()?
            .set_label(self.tenant(), self.session(), label)
            .await?;
        Ok(())
    }

    pub async fn events(&self) -> StoreResult<Vec<StoredEvent>> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        store.sessions().read(self.tenant(), self.session()).await
    }

    /// A page of the log after `after_seq`, plus whether more remain.
    pub async fn events_after(
        &self,
        after_seq: u64,
        limit: usize,
    ) -> StoreResult<(Vec<StoredEvent>, bool)> {
        let Some(store) = &self.store else {
            return Ok((Vec::new(), false));
        };
        let mut page = store
            .sessions()
            .read_after_limited(self.tenant(), self.session(), after_seq, limit + 1)
            .await?;
        let has_more = page.len() > limit;
        page.truncate(limit);
        Ok((page, has_more))
    }

    /// Folded from the tail: a `StateSnapshot` carries the rolled-up totals and
    /// replaces rather than accumulates, so reading past one changes nothing.
    pub async fn stats(&self) -> StoreResult<SessionStats> {
        let Some(store) = &self.store else {
            return Ok(SessionStats::default());
        };
        let stored = store
            .sessions()
            .read_tail(self.tenant(), self.session())
            .await?;
        let mut stats = SessionStats::default();
        for entry in stored {
            stats.fold(&entry.event.lift());
        }
        Ok(stats)
    }

    pub async fn awaiting(&self) -> StoreResult<Option<Deferral>> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let stored = store
            .sessions()
            .read_tail(self.tenant(), self.session())
            .await?;
        let mut state = runic_state::AgentState::new(self.tenant(), self.session(), "");
        for entry in stored {
            state.fold(&entry.event.lift());
        }
        Ok(state.take_pending())
    }

    /// This session and every session delegated from it, parents before children.
    /// Reverse it to delete: a child outliving its parent is an orphan, the
    /// other way round is just a partial delete.
    pub async fn descendants(&self) -> StoreResult<Vec<String>> {
        let store = self.require_store()?;
        let mut pending = vec![self.session().to_string()];
        let mut order: Vec<String> = Vec::new();
        while let Some(session) = pending.pop() {
            let mut cursor = None;
            loop {
                let page = store
                    .list_sessions_page(
                        self.tenant(),
                        cursor,
                        CHILD_PAGE,
                        SessionScope::ChildrenOf(session.clone()),
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
            order.push(session);
        }
        Ok(order)
    }

    pub async fn delete_tree(&self) -> StoreResult<usize> {
        let store = self.require_store()?;
        let order = self.descendants().await?;
        for session in order.iter().rev() {
            store.delete_session(self.tenant(), session).await?;
        }
        Ok(order.len())
    }

    pub async fn messages(&self) -> StoreResult<Vec<Message>> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        replay_messages(store.sessions().as_ref(), self.tenant(), self.session()).await
    }

    pub async fn delete(&self) -> StoreResult<()> {
        self.require_store()?
            .delete_session(self.tenant(), self.session())
            .await?;
        Ok(())
    }

    pub async fn invoke(&self, agent: &Agent, input: Input) -> anyhow::Result<AgentOutput> {
        let (mut message, ctx) = input.split();
        self.admit(&mut message).await?;
        self.drive(agent, Some(message), ctx).await
    }

    async fn admit(&self, message: &mut Message) -> anyhow::Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let MessageContent::Blocks(blocks) = &mut message.content else {
            return Ok(());
        };

        let artifacts = store.artifacts();
        let mut owned: Option<Vec<String>> = None;
        for block in blocks.iter_mut() {
            let Some((media_type, source)) = attachment_mut(block) else {
                continue;
            };
            match source {
                Source::Inline(bytes) => {
                    let artifact = artifacts
                        .put(
                            self.tenant(),
                            self.session(),
                            media_type,
                            ArtifactSource::UserUpload,
                            bytes,
                        )
                        .await?;
                    *source = Source::Stored(artifact.id);
                }
                Source::Stored(id) => {
                    let owned = match &owned {
                        Some(owned) => owned,
                        None => owned.insert(
                            artifacts
                                .list(self.tenant(), self.session())
                                .await?
                                .into_iter()
                                .map(|artifact| artifact.id)
                                .collect(),
                        ),
                    };
                    if !owned.iter().any(|held| held == id) {
                        anyhow::bail!("artifact {id} does not belong to this session");
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub async fn resume(&self, agent: &Agent, input: Input) -> anyhow::Result<AgentOutput> {
        let (_, ctx) = input.split();
        self.drive(agent, None, ctx).await
    }

    async fn drive(
        &self,
        agent: &Agent,
        message: Option<Message>,
        mut ctx: RunContext,
    ) -> anyhow::Result<AgentOutput> {
        let span = tracing::info_span!(
            "session_run",
            tenant = %self.tenant(),
            session = %self.session(),
            persisted = self.store.is_some(),
            persist_backlog_at_flush = tracing::field::Empty,
            flush_ms = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        let result = async {
            let hydrate_span = tracing::info_span!(
                "hydrate",
                tenant = %self.tenant(),
                session = %self.session(),
                events = tracing::field::Empty,
            );
            let mut runner = async {
                let mut bound = agent.clone();
                if let Some(store) = &self.store {
                    bound = bound.tools(store.tools());
                    bound = bound.hooks(store.hooks());
                    bound = bound.hook(ArtifactResolver::new(store.artifacts()));
                }
                let mut runner = bound.build(self.tenant(), self.session()).await?;
                if let Some(store) = &self.store {
                    let entries = store
                        .sessions()
                        .read_tail(self.tenant(), self.session())
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

            let Some(store) = &self.store else {
                let outcome = step(&mut runner, message, ctx)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                return Ok(AgentOutput::from_run(&runner, outcome));
            };

            let (emitter, handle) = attach_persister(
                store.sessions(),
                self.tenant().to_string(),
                self.session().to_string(),
            );
            ctx.events.push(emitter);
            if ctx.sub_session.is_none() {
                ctx.sub_session = Some(Arc::new(StoreSubSession::new(
                    store.sessions(),
                    self.tenant().to_string(),
                    self.session().to_string(),
                )));
            }
            // The runner drops its subscribers when the run ends, so keep a
            // copy: the flush outcome is reported after that.
            let subscribers = ctx.events.clone();

            let run_id = ctx.run_id.clone();
            let outcome = step(&mut runner, message, ctx)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            let current = tracing::Span::current();
            current.record("persist_backlog_at_flush", handle.backlog());
            let started = std::time::Instant::now();
            let flushed = handle.flush().await;
            current.record("flush_ms", started.elapsed().as_millis() as u64);

            let event = AgentEvent::Persisted {
                run_id: run_id
                    .or_else(|| runner.state().current_run_id().map(str::to_string))
                    .unwrap_or_default(),
                status: match &flushed {
                    Ok(()) => PersistenceStatus::Flushed,
                    Err(error) => PersistenceStatus::FlushFailed(error.to_string()),
                },
                at: chrono::Utc::now(),
            };
            for subscriber in &subscribers {
                subscriber.emit(event.clone());
            }
            flushed?;

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
