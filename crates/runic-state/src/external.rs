use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::event::SessionEvent;
use crate::state::PersistSink;

#[derive(Clone)]
pub struct ExternalEvents {
    persist: Option<PersistSink>,
    broadcast: Option<broadcast::Sender<Arc<SessionEvent>>>,
    pending: Arc<Mutex<Vec<SessionEvent>>>,
}

impl ExternalEvents {
    pub fn new(
        persist: Option<PersistSink>,
        broadcast: Option<broadcast::Sender<Arc<SessionEvent>>>,
        pending: Arc<Mutex<Vec<SessionEvent>>>,
    ) -> Self {
        Self {
            persist,
            broadcast,
            pending,
        }
    }

    /// Durable first, live second, folded into warm state at the next turn.
    pub fn emit(&self, ev: SessionEvent) {
        if self.persist.is_some() || self.broadcast.is_some() {
            let shared = Arc::new(ev.clone());
            if let Some(sink) = &self.persist {
                sink.send(shared.clone());
            }
            if let Some(tx) = &self.broadcast {
                let _ = tx.send(shared);
            }
        }
        self.pending.lock().unwrap().push(ev);
    }
}
