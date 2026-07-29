use crate::subagent::{DelegateTool, Subagent};
use runic_skills::SkillSet;
use runic_tool::{Tool, ToolCatalog};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use super::scope::PendingHooks;
use super::view::{AbilityView, SkillInfo, SubagentInfo};
use super::{Agent, ComposeError, Composition, Runtime};
use crate::ability::{Ability, ActivationPolicy, BuildCtx, Layer, ToAbility};
use crate::artifact_resolver::ArtifactResolver;
use crate::deferred::{
    AbilityRegistry, DeferredEntry, GatedTool, LOAD_ABILITY_TOOL_NAME, LoadAbilityTool,
    LoadedAbilities, delegate_subjects, skill_subjects,
};

fn run_boundary_point(point: &runic_hook::HookLifecycle) -> Option<&'static str> {
    match point {
        runic_hook::HookLifecycle::BeforeAgent => Some("before_agent"),
        runic_hook::HookLifecycle::AfterAgent => Some("after_agent"),
        _ => None,
    }
}

fn validate_ability_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let valid_edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();

    !bytes.is_empty()
        && bytes.len() <= 64
        && valid_edge(bytes[0])
        && valid_edge(bytes[bytes.len() - 1])
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(*byte, b'-' | b'_' | b'.')
        })
}

fn validate_ability_descriptors(abilities: &[Arc<dyn ToAbility>]) -> Result<(), ComposeError> {
    let mut to_track_ids: HashMap<String, String> = HashMap::new();
    to_track_ids.insert(
        super::agent::AGENT_BUNDLE_ID.to_string(),
        super::agent::AGENT_BUNDLE_ID.to_string(),
    );
    for ability in abilities {
        let ability_description = ability.descriptor();
        let ability_name = ability.name();
        if ability_description.activation == ActivationPolicy::Deferred
            && ability_description.id.is_none()
        {
            return Err(ComposeError::DeferredAbilityMissingId {
                ability: ability_name.to_string(),
            });
        }
        let Some(id) = ability_description.id else {
            continue;
        };
        if !validate_ability_id(&id) {
            return Err(ComposeError::InvalidAbilityId {
                ability: ability_name.to_string(),
                id,
            });
        }
        if let Some(first_ability) = to_track_ids.get(&id) {
            return Err(ComposeError::DuplicateAbilityId {
                first_ability: first_ability.clone(),
                second_ability: ability_name.to_string(),
                id,
            });
        }
        to_track_ids.insert(id, ability_name.to_string());
    }
    Ok(())
}

struct ChainCatalog(Vec<Arc<dyn ToolCatalog>>);

impl ToolCatalog for ChainCatalog {
    fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.0.iter().find_map(|catalog| catalog.resolve(name))
    }
}

fn into_catalog(mut catalogs: Vec<Arc<dyn ToolCatalog>>) -> Option<Arc<dyn ToolCatalog>> {
    match catalogs.len() {
        0 => None,
        1 => catalogs.pop(),
        _ => Some(Arc::new(ChainCatalog(catalogs))),
    }
}

pub struct Composer {
    agent: Agent,
    runtime: Runtime,
    activated: HashSet<String>,
}

impl Composer {
    pub fn new(agent: Agent, runtime: Runtime) -> Self {
        Self {
            agent,
            runtime,
            activated: HashSet::new(),
        }
    }

    pub fn activated<Ids, Id>(mut self, ids: Ids) -> Self
    where
        Ids: IntoIterator<Item = Id>,
        Id: Into<String>,
    {
        self.activated.extend(ids.into_iter().map(Into::into));
        self
    }

    pub async fn build(
        &self,
        tenant: &str,
        session: &str,
    ) -> Result<runic_agent::Runner, ComposeError> {
        validate_ability_descriptors(&self.agent.abilities)?;
        let reserve_loader_name = self
            .agent
            .abilities
            .iter()
            .any(|ability| ability.descriptor().activation == ActivationPolicy::Deferred);
        let provider = self.agent.llm.provider();
        let model = self.agent.llm.config().model.clone();
        let ctx = BuildCtx {
            tenant,
            session,
            provider: &provider,
            model: &model,
        };
        let mut composition = Composition::default();
        composition
            .prompt
            .instructions(self.agent.llm.system_prompt());
        let mut registry = AbilityRegistry {
            entries: Vec::new(),
        };
        let loaded = LoadedAbilities::seeded(&self.activated);
        let mut eager_owners: HashMap<String, String> = HashMap::new();
        let mut deferred_owners: HashMap<String, String> = HashMap::new();
        let mut skill_owner_names: HashMap<String, String> = HashMap::new();
        let mut subagent_owner_names: HashMap<String, String> = HashMap::new();
        let mut deferred_skill_owners: HashMap<String, String> = HashMap::new();
        let mut deferred_subagent_owners: HashMap<String, String> = HashMap::new();
        let mut pending_hooks: Vec<PendingHooks> = Vec::new();
        let mut agent_hooks: Vec<Arc<dyn runic_hook::WriteHook>> = Vec::new();
        let mut bundles: Vec<(bool, Arc<dyn ToAbility>)> = Vec::new();
        if !self.agent.base.carries_nothing() {
            bundles.push((true, Arc::new(self.agent.base.clone())));
        }
        bundles.extend(
            self.agent
                .abilities
                .iter()
                .map(|ability| (false, ability.clone())),
        );
        for (is_agent_bundle, ability) in &bundles {
            let is_agent_bundle = *is_agent_bundle;
            let ability_name = ability.name().to_string();
            let descriptor = ability.descriptor();
            let mut parts = ability
                .to_ability(Ability::new(ability_name.clone()), &ctx)
                .await
                .map_err(|source| ComposeError::Ability {
                    ability: ability_name.clone(),
                    source,
                })?
                .resolve(&ctx)
                .await
                .map_err(|source| ComposeError::Ability {
                    ability: ability_name.clone(),
                    source,
                })?;
            let ability_hooks = std::mem::take(&mut parts.hooks);
            if !is_agent_bundle {
                for hook in &ability_hooks {
                    if let Some(point) = hook.points().iter().find_map(run_boundary_point) {
                        return Err(ComposeError::AbilityLifecycleHook {
                            ability: ability_name,
                            hook: hook.name().to_string(),
                            point,
                        });
                    }
                }
            }
            let deferred_id = match descriptor.activation {
                ActivationPolicy::Deferred => descriptor
                    .id
                    .filter(|id| !self.activated.contains(id.as_str())),
                ActivationPolicy::Eager => None,
            };
            let mut owned_tools = HashSet::new();
            let mut owned_skills = HashSet::new();
            let mut owned_subagents = HashSet::new();
            for tool in &parts.tools {
                let tool_name = tool.name().to_string();
                owned_tools.insert(tool_name.clone());
                if reserve_loader_name && tool_name == LOAD_ABILITY_TOOL_NAME {
                    return Err(ComposeError::ReservedToolName {
                        ability: ability_name,
                    });
                }
                let colliding_owner = if deferred_id.is_some() {
                    eager_owners
                        .get(&tool_name)
                        .or_else(|| deferred_owners.get(&tool_name))
                } else {
                    deferred_owners.get(&tool_name)
                };
                if let Some(first_ability) = colliding_owner {
                    return Err(ComposeError::DuplicateToolName {
                        first_ability: first_ability.clone(),
                        second_ability: ability_name,
                        tool: tool_name,
                    });
                }
                if deferred_id.is_some() {
                    deferred_owners.insert(tool_name, ability_name.clone());
                } else {
                    eager_owners.insert(tool_name, ability_name.clone());
                }
            }
            for set in &parts.skills {
                for skill_id in set.ids() {
                    if let Some(first_ability) = skill_owner_names.get(&skill_id) {
                        return Err(ComposeError::DuplicateSkillId {
                            first_ability: first_ability.clone(),
                            second_ability: ability_name,
                            id: skill_id,
                        });
                    }
                    skill_owner_names.insert(skill_id.clone(), ability_name.clone());
                    owned_skills.insert(skill_id.clone());
                    if let Some(id) = &deferred_id {
                        deferred_skill_owners.insert(skill_id, id.clone());
                    }
                }
            }
            for def in &parts.subagents {
                if let Some(first_ability) = subagent_owner_names.get(&def.name) {
                    return Err(ComposeError::DuplicateSubagentName {
                        first_ability: first_ability.clone(),
                        second_ability: ability_name,
                        name: def.name.clone(),
                    });
                }
                subagent_owner_names.insert(def.name.clone(), ability_name.clone());
                owned_subagents.insert(def.name.clone());
                if let Some(id) = &deferred_id {
                    deferred_subagent_owners.insert(def.name.clone(), id.clone());
                }
            }
            if is_agent_bundle {
                agent_hooks.extend(ability_hooks);
            } else if !ability_hooks.is_empty() {
                pending_hooks.push(PendingHooks {
                    ability: ability_name.clone(),
                    deferred_id: deferred_id.clone(),
                    hooks: ability_hooks,
                    tools: owned_tools,
                    subagents: owned_subagents,
                    skills: owned_skills,
                });
            }
            match deferred_id {
                Some(id) => registry.entries.push(DeferredEntry {
                    id,
                    description: descriptor.description.unwrap_or_default(),
                    parts,
                }),
                None => composition.merge(parts),
            }
        }

        let mut skill_tool_name: Option<String> = None;
        let mut delegate_tool_name: Option<String> = None;

        let deferred_skills: Vec<Arc<SkillSet>> = registry
            .entries
            .iter()
            .flat_map(|entry| entry.parts.skills.iter().cloned())
            .collect();
        if !composition.skills.is_empty() || !deferred_skills.is_empty() {
            let visible = SkillSet::merge(composition.skills.iter().cloned());
            if !visible.is_empty() {
                composition
                    .prompt
                    .fragment(Layer::Stable, visible.prompt_section());
            }
            let full = Arc::new(SkillSet::merge(
                composition.skills.iter().cloned().chain(deferred_skills),
            ));
            if let Some(view) = full.view_tool() {
                skill_tool_name = Some(view.name().to_string());
                composition.tools.push(Arc::new(GatedTool::new(
                    view,
                    loaded.clone(),
                    deferred_skill_owners,
                    "Skill",
                    skill_subjects,
                )));
            }
        }

        let deferred_defs: Vec<Subagent> = registry
            .entries
            .iter()
            .flat_map(|entry| entry.parts.subagents.iter().cloned())
            .collect();
        if !composition.subagents.is_empty() || !deferred_defs.is_empty() {
            if !composition.subagents.is_empty() {
                composition.prompt.fragment(
                    Layer::Stable,
                    composition
                        .delegation_voice
                        .roster_section(&composition.subagents),
                );
            }
            let full_roster: Vec<Subagent> = composition
                .subagents
                .iter()
                .cloned()
                .chain(deferred_defs)
                .collect();
            let delegate: Arc<dyn Tool> = Arc::new(
                DelegateTool::new(full_roster).voice(composition.delegation_voice.clone()),
            );
            delegate_tool_name = Some(delegate.name().to_string());
            composition.tools.push(Arc::new(GatedTool::new(
                delegate,
                loaded.clone(),
                deferred_subagent_owners,
                "Subagent",
                delegate_subjects,
            )));
        }

        if reserve_loader_name {
            if !registry.entries.is_empty() {
                composition
                    .prompt
                    .fragment(Layer::Stable, registry.catalog_section());
            }
            let registry = Arc::new(registry);
            composition.tools.push(Arc::new(LoadAbilityTool::new(
                registry.clone(),
                loaded.clone(),
            )));
            composition.tool_catalogs.push(registry);
        }

        let mut tools = composition.tools;
        if let Some(store) = &self.runtime.artifact_store
            && !tools.iter().any(|t| t.name() == "read_thread_artifact")
        {
            tools.push(Arc::new(runic_substrate::ReadThreadArtifactTool::new(
                store.clone(),
            )));
        }
        let mut agent_builder = runic_agent::Runner::builder(provider.clone(), tenant, session)
            .config(self.agent.llm.config().clone())
            .system_prompt(composition.prompt.render());
        for tool in self.agent.llm.tool_list() {
            agent_builder = agent_builder.tool(tool.clone());
        }
        for tool in tools {
            agent_builder = agent_builder.tool(tool);
        }
        for hook in agent_hooks {
            agent_builder = agent_builder.write_hook(hook);
        }
        for pending in pending_hooks {
            let (hooks, scope) = pending.into_scope(
                &loaded,
                delegate_tool_name.as_deref(),
                skill_tool_name.as_deref(),
            );
            for hook in hooks {
                agent_builder = agent_builder.scoped_write_hook(hook, scope.clone());
            }
        }
        if let Some(store) = &self.runtime.artifact_store {
            agent_builder = agent_builder
                .media_resolver(Arc::new(ArtifactResolver::new(
                    store.clone(),
                    tenant,
                    session,
                )))
                .artifact_spill(Arc::new(crate::SpillToArtifacts::new(store.clone())));
        }
        if let Some(bytes) = self.runtime.auto_spill_over {
            agent_builder = agent_builder.auto_spill_over(bytes);
        }
        if let Some(catalog) = into_catalog(composition.tool_catalogs) {
            agent_builder = agent_builder.tool_catalog(catalog);
        }
        if let Some(schema) = &self.agent.output_schema {
            agent_builder = agent_builder.output_schema(schema.clone());
        }
        Ok(agent_builder.build())
    }

    pub async fn describe(
        &self,
        tenant: &str,
        session: &str,
    ) -> Result<Vec<AbilityView>, ComposeError> {
        validate_ability_descriptors(&self.agent.abilities)?;
        let provider = self.agent.llm.provider();
        let model = self.agent.llm.config().model.clone();
        let ctx = BuildCtx {
            tenant,
            session,
            provider: &provider,
            model: &model,
        };
        let mut views = Vec::new();
        let mut bundles: Vec<Arc<dyn ToAbility>> = Vec::new();
        if !self.agent.base.carries_nothing() {
            bundles.push(Arc::new(self.agent.base.clone()));
        }
        bundles.extend(self.agent.abilities.iter().cloned());
        for ability in &bundles {
            let ability_name = ability.name().to_string();
            let descriptor = ability.descriptor();
            let parts = ability
                .to_ability(Ability::new(ability_name.clone()), &ctx)
                .await
                .map_err(|source| ComposeError::Ability {
                    ability: ability_name.clone(),
                    source,
                })?
                .resolve(&ctx)
                .await
                .map_err(|source| ComposeError::Ability {
                    ability: ability_name.clone(),
                    source,
                })?;
            let activated = match descriptor.activation {
                ActivationPolicy::Eager => true,
                ActivationPolicy::Deferred => descriptor
                    .id
                    .as_deref()
                    .is_some_and(|id| self.activated.contains(id)),
            };
            let skills: Vec<SkillInfo> = parts
                .skills
                .iter()
                .flat_map(|set| {
                    set.ids().into_iter().filter_map(move |id| {
                        set.get(&id).map(|skill| SkillInfo {
                            id,
                            description: skill.description.clone(),
                        })
                    })
                })
                .collect();
            let subagents: Vec<SubagentInfo> = parts
                .subagents
                .iter()
                .map(|def| SubagentInfo {
                    name: def.name.clone(),
                    description: def.description.clone(),
                })
                .collect();
            let hooks: Vec<String> = parts
                .hooks
                .iter()
                .map(|hook| hook.name().to_string())
                .collect();
            views.push(AbilityView {
                id: descriptor.id,
                name: ability_name,
                description: descriptor.description,
                deferred: descriptor.activation == ActivationPolicy::Deferred,
                activated,
                prompt: parts.prompt,
                tools: parts.tools.iter().map(|tool| tool.spec()).collect(),
                skills,
                subagents,
                hooks,
            });
        }
        Ok(views)
    }
}
