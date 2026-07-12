pub mod ability;
mod artifact_resolver;
mod child;
pub mod composer;
mod context;
pub mod deferred;
pub mod hooks;
mod models;
pub mod output;
pub mod tools;

pub use ability::{Ability, AbilityBundle, ability};
pub use artifact_resolver::ArtifactResolver;
pub use child::FoundrySubagentBuilder;
pub use composer::{Compose, ComposeError, Composer};
pub use context::Context;
pub use hooks::Compaction;
pub use output::StructuredOutput;
pub use runic_macros::tool;

#[doc(hidden)]
pub mod __private {
    pub use anyhow;
    pub use async_trait::async_trait;
    pub use serde_json;
}

pub mod agent {
    pub use runic_agent::*;
}
pub mod commands {
    pub use runic_commands::*;
}
pub mod hook {
    pub use runic_hook::*;
}
pub mod mcp {
    pub use runic_mcp::*;
}
pub mod memory {
    pub use runic_memory::*;
}
pub mod provider {
    pub use runic_provider::*;
}
pub mod skills {
    pub use runic_skills::*;
}
pub mod state {
    pub use runic_state::*;
}
pub mod subagent {
    pub use runic_subagent::*;
}
pub mod substrate {
    pub use runic_substrate::*;
}
pub mod tool {
    pub use runic_tool::*;
}
pub mod transcriber {
    pub use runic_transcriber::*;
}
pub mod types {
    pub use runic_types::*;
}
