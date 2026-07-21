use std::sync::{Arc, Mutex};

use runic_agent::RunContext;
use runic_state::{AgentEvent, Emitter};
use runic_substrate::{SessionStore, project};

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

        let sink = Arc::new(Collector::default());
        let ctx = RunContext::new().with_events(sink.clone());
        let outcome = runner
            .run_with(message.into(), ctx)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let events: Vec<_> = sink.take().iter().filter_map(project).collect();
        if !events.is_empty() {
            self.store
                .append_batch(&self.tenant, &self.session_id, &events)
                .await?;
        }

        Ok(AgentOutput::from_run(&runner, outcome))
    }
}

#[derive(Default, Debug)]
struct Collector(Mutex<Vec<AgentEvent>>);

impl Emitter for Collector {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Collector {
    fn take(&self) -> Vec<AgentEvent> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}
