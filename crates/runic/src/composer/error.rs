#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ComposeError {
    #[error("ability `{ability}` failed: {source}")]
    Ability {
        ability: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("deferred ability `{ability}` requires a stable ID")]
    DeferredAbilityMissingId { ability: String },
    #[error("ability `{ability}` has an invalid ID: {id}")]
    InvalidAbilityId { ability: String, id: String },
    #[error("ability ID `{id}` is declared by both `{first_ability}` and `{second_ability}`")]
    DuplicateAbilityId {
        first_ability: String,
        second_ability: String,
        id: String,
    },
    #[error(
        "tool `{tool}` from `{second_ability}` collides with a tool from `{first_ability}` across the deferred boundary"
    )]
    DuplicateToolName {
        first_ability: String,
        second_ability: String,
        tool: String,
    },
    #[error(
        "tool name `load_ability` is reserved for the ability loader (declared by `{ability}`)"
    )]
    ReservedToolName { ability: String },
    #[error("skill `{id}` is declared by both `{first_ability}` and `{second_ability}`")]
    DuplicateSkillId {
        first_ability: String,
        second_ability: String,
        id: String,
    },
    #[error("subagent `{name}` is declared by both `{first_ability}` and `{second_ability}`")]
    DuplicateSubagentName {
        first_ability: String,
        second_ability: String,
        name: String,
    },
    #[error(
        "hook `{hook}` on ability `{ability}` declares `{point}`, which is not an ability-scoped moment — an ability's hook is live only while that ability is in play, and the run boundary is outside every ability. Narrow `points()` to the model and tool points, or move the hook to `Agent::hook` to make it agent-wide"
    )]
    AbilityLifecycleHook {
        ability: String,
        hook: String,
        point: &'static str,
    },
    #[error("model spec `{spec}` is not `provider:model`")]
    InvalidModelSpec { spec: String },
    #[error(
        "unknown provider `{name}` (enable its feature on the `runic` crate or pass a provider to `Composer::new`)"
    )]
    UnknownProvider { name: String },
    #[error("provider `{provider}` needs the `{env_var}` environment variable")]
    MissingApiKey {
        provider: &'static str,
        env_var: &'static str,
    },
}
