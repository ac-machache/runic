//! The `memory(...)` builder — resolve a per-tenant [`BoundedMemoryStore`] and
//! the `memory` tool. The background-review hook lives in the wiring layer (it
//! needs the agent loop, which this crate must not depend on); this builder
//! just records the review interval via [`Memory::review_interval`].

use std::path::PathBuf;
use std::sync::{Arc, Once};

use runic_filesystem::{FilesystemBackend, LocalFs};
use runic_tool::Tool;

use crate::{BoundedMemoryStore, MemoryTool};

static LOCK_WARN: Once = Once::new();

pub fn memory(path: impl Into<PathBuf>) -> Memory {
    Memory {
        path: path.into(),
        scoped: false,
        mem_tools: false,
        create: false,
        review: 0,
    }
}

pub struct Memory {
    path: PathBuf,
    scoped: bool,
    mem_tools: bool,
    create: bool,
    review: u32,
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
    pub fn include_mem_tools(mut self) -> Self {
        self.mem_tools = true;
        self
    }
    /// Run a background memory-review curator every `every_n_turns` user turns
    /// (hermes-style reflection). `0` (default) disables it. The wiring layer
    /// reads this via [`Memory::review_interval`].
    pub fn review(mut self, every_n_turns: u32) -> Self {
        self.review = every_n_turns;
        self
    }

    /// The configured review interval (`0` = disabled).
    pub fn review_interval(&self) -> u32 {
        self.review
    }

    pub fn store(&self, tenant: &str) -> Arc<BoundedMemoryStore> {
        tracing::info!(
            root = %self.path.display(),
            scoped = self.scoped,
            mem_tools = self.mem_tools,
            init = self.create,
            review = self.review,
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

        LOCK_WARN.call_once(|| {
            tracing::warn!(
                "memory has no cross-process lock (with_lock_dir unset) — concurrent processes may clobber writes"
            );
        });

        if self.create
            && let Err(e) = std::fs::create_dir_all(&dir)
        {
            tracing::error!(dir = %dir.display(), error = %e, "failed to create memory dir");
        }

        let fs: Arc<dyn FilesystemBackend> = Arc::new(LocalFs::new(dir));
        Arc::new(BoundedMemoryStore::new(fs))
    }

    pub fn tools(&self, store: Arc<BoundedMemoryStore>) -> Option<Arc<dyn Tool>> {
        if self.mem_tools {
            tracing::debug!("memory tool enabled");
            Some(Arc::new(MemoryTool::new(store)) as Arc<dyn Tool>)
        } else {
            None
        }
    }
}
