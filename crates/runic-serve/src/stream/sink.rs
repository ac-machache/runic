use std::sync::Arc;

use runic_state::{AgentEvent, Emitter};

use crate::wire::{WireEvent, from_agent_event};

#[derive(Debug, Default)]
pub struct Replay {
    pub events: Vec<(u64, WireEvent)>,
    pub gap: bool,
    pub closed: bool,
}

#[async_trait::async_trait]
pub trait RunEvents: Send + Sync + 'static {
    fn publish(&self, run_id: &str, event: WireEvent);

    fn finish(&self, run_id: &str);

    async fn since(&self, run_id: &str, after: u64) -> Replay;
}

pub struct RunEmitter {
    run_id: String,
    events: Arc<dyn RunEvents>,
}

impl RunEmitter {
    pub fn new(run_id: impl Into<String>, events: Arc<dyn RunEvents>) -> Self {
        Self {
            run_id: run_id.into(),
            events,
        }
    }
}

impl std::fmt::Debug for RunEmitter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunEmitter")
            .field("run_id", &self.run_id)
            .finish()
    }
}

impl Emitter for RunEmitter {
    fn emit(&self, event: AgentEvent) {
        for wire in from_agent_event(event) {
            if matches!(wire, WireEvent::Done { .. }) {
                continue;
            }
            self.events.publish(&self.run_id, wire);
        }
    }
}
