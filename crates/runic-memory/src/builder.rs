use std::path::PathBuf;
use std::sync::Arc;

use runic_tool::Tool;

use crate::storage::LocalStorage;
use crate::{MemoryStore, MemoryTool};

pub fn memory(path: impl Into<PathBuf>) -> Memory {
    Memory {
        path: path.into(),
        scoped: false,
        tool: false,
        tool_description: None,
        create: false,
        curate_every_turns: 0,
        curation_guidance: None,
    }
}

pub struct Memory {
    path: PathBuf,
    scoped: bool,
    tool: bool,
    tool_description: Option<String>,
    create: bool,
    curate_every_turns: u32,
    curation_guidance: Option<String>,
}

impl Memory {
    pub fn init(mut self) -> Self {
        self.create = true;
        self
    }
    pub fn scope_per_tenant(mut self) -> Self {
        self.scoped = true;
        self
    }
    pub fn include_memory_tool(mut self) -> Self {
        self.tool = true;
        self
    }
    pub fn memory_tool_description(mut self, description: impl Into<String>) -> Self {
        self.tool_description = Some(description.into());
        self
    }
    pub fn curate_every_turns(mut self, turns: u32) -> Self {
        self.curate_every_turns = turns;
        self
    }

    pub fn curation_guidance(mut self, guidance: impl Into<String>) -> Self {
        self.curation_guidance = Some(guidance.into());
        self
    }

    pub fn curation_interval_turns(&self) -> u32 {
        self.curate_every_turns
    }

    pub fn curation_guidance_override(&self) -> Option<&str> {
        self.curation_guidance.as_deref()
    }

    pub async fn store(&self, tenant: &str) -> Arc<MemoryStore> {
        tracing::info!(
            root = %self.path.display(),
            scoped = self.scoped,
            tool = self.tool,
            init = self.create,
            curate_every_turns = self.curate_every_turns,
            "configuring memory"
        );

        let dir = if self.scoped {
            self.path.join(tenant)
        } else {
            self.path.clone()
        };
        tracing::debug!(tenant, dir = %dir.display(), "resolved memory path");

        if self.scoped && tenant.is_empty() {
            tracing::warn!(
                "scope_per_tenant set but tenant is empty — memory is shared, not isolated"
            );
        }

        if self.create
            && let Err(e) = tokio::fs::create_dir_all(&dir).await
        {
            tracing::error!(dir = %dir.display(), error = %e, "failed to create memory dir");
        }

        let storage = Arc::new(LocalStorage::new(&dir));
        Arc::new(MemoryStore::new(storage).with_lock_dir(dir))
    }

    pub fn tools(&self, store: Arc<MemoryStore>) -> Option<Arc<dyn Tool>> {
        if self.tool {
            tracing::debug!("memory tool enabled");
            let mut tool = MemoryTool::new(store);
            if let Some(description) = &self.tool_description {
                tool = tool.with_description(description.clone());
            }
            Some(Arc::new(tool) as Arc<dyn Tool>)
        } else {
            None
        }
    }
}
