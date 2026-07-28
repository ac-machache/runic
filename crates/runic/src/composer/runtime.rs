use std::sync::Arc;

use runic_hook::WriteHook;
use runic_substrate::ArtifactStore;

#[derive(Default)]
pub struct Runtime {
    pub(crate) hooks: Vec<Arc<dyn WriteHook>>,
    pub(crate) artifact_store: Option<Arc<dyn ArtifactStore>>,
    pub(crate) auto_spill_over: Option<usize>,
}

impl Runtime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn hook(mut self, hook: impl WriteHook + 'static) -> Self {
        self.hooks.push(Arc::new(hook));
        self
    }

    pub fn hooks(mut self, hooks: impl IntoIterator<Item = Arc<dyn WriteHook>>) -> Self {
        self.hooks.extend(hooks);
        self
    }

    pub fn artifacts(mut self, store: Arc<dyn ArtifactStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    pub fn auto_spill_over(mut self, bytes: usize) -> Self {
        self.auto_spill_over = Some(bytes);
        self
    }
}
