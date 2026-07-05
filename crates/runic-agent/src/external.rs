use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use runic_state::TaskRecord;

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
