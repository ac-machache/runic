use std::sync::Arc;

use async_trait::async_trait;
use runic_provider::Provider;

use super::Ability;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivationPolicy {
    #[default]
    Eager,
    Deferred,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbilityDescriptor {
    pub id: Option<String>,
    pub description: Option<String>,
    pub activation: ActivationPolicy,
}

impl AbilityDescriptor {
    pub fn eager() -> Self {
        Self {
            id: None,
            description: None,
            activation: ActivationPolicy::Eager,
        }
    }

    pub fn deferred(id: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            description: Some(description.into()),
            activation: ActivationPolicy::Deferred,
        }
    }
}

pub struct BuildCtx<'a> {
    pub tenant: &'a str,
    pub session: &'a str,
    pub provider: &'a Arc<dyn Provider>,
    pub model: &'a str,
}

/// Adds to an [`Ability`]. `#[ability]` writes this impl; the composer hands
/// you a base already carrying your identity.
#[async_trait]
pub trait ToAbility: Send + Sync {
    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }

    fn descriptor(&self) -> AbilityDescriptor {
        AbilityDescriptor::eager()
    }

    async fn to_ability(&self, base: Ability, ctx: &BuildCtx<'_>) -> anyhow::Result<Ability>;
}
