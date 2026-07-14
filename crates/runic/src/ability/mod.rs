mod builtin;
mod bundle;
mod def;
mod draft;
pub(crate) mod mcp;
pub(crate) mod subagent;

pub use crate::context::Layer;
pub use builtin::{
    Compaction, Delegation, Hooks, Memory, Sessions, Skills, Tools, ask_user, basics, composio,
    weather, web_fetch, web_search,
};
pub use bundle::AbilityBundle;
pub use def::{Ability, AbilityDescriptor, ActivationPolicy, BuildCtx};
pub use draft::{AbilityDraft, ability};
pub use mcp::{McpDeferred, McpDirect};
pub use subagent::{SubagentDraft, subagent};
