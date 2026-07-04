use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use runic_state::{PersistSink, SessionEvent, TaskRecord};
use tokio::sync::broadcast;

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

#[derive(Clone)]
pub struct TasksSnapshot(pub Arc<HashMap<String, TaskRecord>>);

#[derive(Clone, Default)]
pub struct ReminderQueue(Arc<Mutex<Vec<String>>>);

impl ReminderQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, text: impl Into<String>) {
        self.0.lock().unwrap().push(text.into());
    }

    pub fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
    }
}
