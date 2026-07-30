use std::sync::Arc;

use runic_state::{AgentEvent, Emitter};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct ChannelEmitter(pub mpsc::UnboundedSender<AgentEvent>);

impl Emitter for ChannelEmitter {
    fn emit(&self, event: AgentEvent) {
        let _ = self.0.send(event);
    }
}

#[derive(Debug, Clone)]
pub struct ToolEmitter {
    pub sinks: Vec<Arc<dyn Emitter>>,
    pub fold: mpsc::UnboundedSender<AgentEvent>,
}

impl Emitter for ToolEmitter {
    fn emit(&self, event: AgentEvent) {
        for sink in &self.sinks {
            sink.emit(event.clone());
        }
        let _ = self.fold.send(event);
    }
}
