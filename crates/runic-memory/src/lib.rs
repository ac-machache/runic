//! Bounded curated memory plus the `memory` tool.
//!
//! Stores `MEMORY.md` and `USER.md` as delimiter-separated entries with size
//! caps, threat scanning, drift detection, and cross-process locking.

pub mod builder;
pub mod config;
pub mod error;
pub mod lock;
pub mod manager;
pub mod provider;
pub mod review;
pub mod storage;
pub mod store;
pub mod threats;
pub mod tool;

pub use builder::{Memory, memory};
pub use config::{DEFAULT_NUDGE_INTERVAL, ExternalProviderConfig, MemoryConfig, ProviderConfig};
pub use error::MemoryError;
pub use manager::MemoryManager;
pub use provider::{BuiltinProvider, MemoryProvider, MemoryScope, MemoryWriteMeta};
pub use review::{MEMORY_REVIEW_GUIDANCE, ReviewScheduler};
pub use storage::{
    LocalStorage, MemStorage, MemoryObject, MemoryRevision, MemoryStorage, MemoryStorageError,
};
pub use store::{
    DEFAULT_MEMORY_LIMIT, DEFAULT_USER_LIMIT, ENTRY_DELIMITER, MEMORY_KEY, MemorySnapshot,
    MemoryStore, Target, USER_KEY, render_block,
};
pub use threats::ThreatHit;
pub use tool::{DEFAULT_MEMORY_TOOL_DESCRIPTION, MemoryTool};
