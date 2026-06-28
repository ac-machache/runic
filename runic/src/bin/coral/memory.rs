//! Coral's memory definition — the local builtin file store (MEMORY.md +
//! USER.md), one place to tune every option the `memory()` builder offers.
//!
//! Options available:
//! - `.init()`              — create the store dir if missing.
//! - `.scope_per_tenant()`  — isolate memory per tenant (subdir per tenant).
//! - `.include_mem_tools()` — register the `memory` tool (read/add/remove/replace).
//! - `.review(n)`           — background curator every `n` turns (0 = off).

use runic_memory::{Memory, memory};

/// Root dir for the file store. Per-tenant subdirs are created under it when
/// `scope_per_tenant` is on. `${CORAL_MEMORY_DIR}` overrides.
const DIR: &str = "/Users/machache/learner/runic/memory_home/memories";

/// Background memory-review curator: turns between auto-curations. 0 = off.
const REVIEW_INTERVAL: u32 = 10;

pub fn coral_memory() -> Memory {
    let dir = std::env::var("CORAL_MEMORY_DIR").unwrap_or_else(|_| DIR.to_string());
    memory(dir)
        .init()
        .scope_per_tenant()
        .include_mem_tools()
        .review(REVIEW_INTERVAL)
}
