use std::collections::HashMap;
use std::sync::Arc;

use super::def::Routine;

#[derive(Default, Clone)]
pub struct RoutineRegistry(HashMap<String, Arc<dyn Routine>>);

impl RoutineRegistry {
    pub fn insert(&mut self, name: impl Into<String>, routine: Arc<dyn Routine>) {
        self.0.insert(name.into(), routine);
    }

    pub fn knows(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Routine>> {
        self.0.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}
