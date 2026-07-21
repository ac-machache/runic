use std::sync::Arc;

use runic_agent::RunContext;
use runic_substrate::{SessionStore, StoreSubSession, attach_persister};

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
        let mut runner = self.agent.build(&self.tenant, &self.session_id).await?;

        for entry in self.store.read(&self.tenant, &self.session_id).await? {
            runner.state_mut().fold(&entry.event.lift());
        }

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
            .run_with(message.into(), ctx)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        handle.flush().await?;

        Ok(AgentOutput::from_run(&runner, outcome))
    }
}
