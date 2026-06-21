//! `runic-memory` — bounded, file-backed curated memory + a `memory` tool.
//!
//! Two stores, both held as `\n§\n`-delimited entries in markdown files
//! under the `StorageBackend`:
//!
//! - `memory/MEMORY.md` — the agent's own notes (env facts, project
//!   conventions, tool quirks, things learned). Default cap: 2200 chars.
//! - `memory/USER.md`   — facts about the user (preferences, style,
//!   workflow). Default cap: 1375 chars.
//!
//! Caps are TOTAL characters across all entries (including the delimiters).
//! Writes that would breach the cap are rejected — the agent must `remove`
//! or `replace` first. Same shape as the hermes pattern, no surprises.
//!
//! ## Production hardening (all on by default)
//!
//! - **Threat scanning** (`threats`): rejects entries containing
//!   prompt-injection phrasing, shell-exfil patterns, persistence
//!   backdoors, or invisible Unicode before they hit disk.
//! - **Drift detection** (`store`): on every write, re-reads the file
//!   first; if its content wouldn't round-trip through the parser, takes
//!   a `.bak.<unix-ts>` snapshot and refuses the mutation so a stale
//!   patch isn't silently overwritten.
//! - **Cross-process lock** (`lock`): sidecar `.lock` file + `fcntl::flock`
//!   on Unix wraps every RMW. Composes with the in-process tokio mutex.
//! - **Frozen layers**: pair this crate with `MemoryLayer::frozen()` /
//!   `UserFactsLayer::frozen()` in `runic-context-engine` so the system
//!   prompt stays stable for the whole session (prefix-cache friendly).
//!   Mid-session writes still land on disk; the next session picks them up.

pub mod config;
pub mod error;
pub mod lock;
pub mod manager;
pub mod provider;
pub mod review;
pub mod store;
pub mod threats;
pub mod tool;

pub use config::{DEFAULT_NUDGE_INTERVAL, ExternalProviderConfig, MemoryConfig, ProviderConfig};
pub use error::MemoryError;
pub use manager::MemoryManager;
pub use provider::{BuiltinProvider, MemoryProvider, MemoryScope, MemoryWriteMeta};
pub use review::{MEMORY_REVIEW_GUIDANCE, ReviewScheduler};
pub use store::{
    BoundedMemoryStore, DEFAULT_MEMORY_LIMIT, DEFAULT_USER_LIMIT, ENTRY_DELIMITER, MEMORY_KEY,
    MemorySnapshot, Target, USER_KEY, render_block,
};
pub use threats::ThreatHit;
pub use tool::MemoryTool;
