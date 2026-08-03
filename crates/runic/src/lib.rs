pub mod ability;
pub mod builtin;
pub mod composer;
mod context;
pub mod deferred;
mod input;
mod models;
pub mod output;

pub use ability::{Ability, ToAbility};
pub use builtin::Compaction;
pub use composer::{
    Agent, AgentDef, AgentOutput, ComposeError, Composer, Session, SessionKey, session,
};
pub use context::Context;
pub use input::Input;
pub use runic_agent::{CancelToken, Llm, LlmOutput, RunContext};

pub fn llm(spec: &str) -> Result<Llm, ComposeError> {
    let (provider, model) = models::infer(spec)?;
    Ok(Llm::new(provider, model))
}
pub use output::StructuredOutput;
pub use runic_macros::{ability, agent, hook, subagent, tool};

#[doc(hidden)]
pub mod __private {
    pub use anyhow;
    pub use async_trait;
    pub use serde_json;
}

pub mod agent {
    pub use runic_agent::*;
}
pub mod hook {
    pub use runic_hook::*;
}
pub mod mcp {
    pub use crate::ability::mcp::{deferred, direct};
    pub use runic_mcp::*;
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
pub mod store {
    pub use runic_store::*;
}
pub mod subagent;
pub mod tool {
    pub use runic_tool::*;
}
pub mod transcriber {
    pub use runic_transcriber::*;
}
pub mod types {
    pub use runic_types::*;
}
