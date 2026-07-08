use std::sync::Arc;

use async_trait::async_trait;
use runic_agent::{Agent, MediaResolver};
use runic_hook::WriteHook;
use runic_mcp::McpConnection;
use runic_memory::Memory as MemoryConfig;
use runic_provider::Provider;
use runic_skills::SkillSet;
use runic_subagent::{SubagentBuilder, Subagents, roster_prompt_section};
use runic_substrate::{ArtifactStore, Sessions as SessionsConfig};
use runic_tool::Tool;
use runic_tool::ToolCatalog;
use runic_tools::Tools as StdTools;

use crate::artifact_resolver::ArtifactResolver;
use crate::child::FoundrySubagentBuilder;
use crate::hooks::{Compaction as CompactionConfig, CompactionHook, MemoryCurator};

pub use crate::context::Layer;

pub struct BuildCtx<'a> {
    pub tenant: &'a str,
    pub session: &'a str,
    pub provider: &'a Arc<dyn Provider>,
    pub model: &'a str,
}

#[derive(Default)]
pub struct Composition {
    prompt: crate::context::Context,
    tools: Vec<Arc<dyn Tool>>,
    write_hooks: Vec<Arc<dyn WriteHook>>,
    media_resolver: Option<Arc<dyn MediaResolver>>,
    tool_catalog: Option<Arc<dyn ToolCatalog>>,
}

impl Composition {
    pub fn prompt(&mut self, layer: Layer, text: impl Into<String>) {
        self.prompt.fragment(layer, text);
    }
    pub fn tool(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }
    pub fn write_hook(&mut self, hook: Arc<dyn WriteHook>) {
        self.write_hooks.push(hook);
    }
    pub fn media_resolver(&mut self, resolver: Arc<dyn MediaResolver>) {
        self.media_resolver = Some(resolver);
    }
    pub fn tool_catalog(&mut self, catalog: Arc<dyn ToolCatalog>) {
        self.tool_catalog = Some(catalog);
    }
}

#[async_trait]
pub trait Ability: Send + Sync {
    async fn contribute(&self, c: &mut Composition, ctx: &BuildCtx<'_>);
}

pub trait Compose {
    fn compose(provider: Arc<dyn Provider>, model: impl Into<String>) -> Composer;
}

impl Compose for Agent {
    fn compose(provider: Arc<dyn Provider>, model: impl Into<String>) -> Composer {
        Composer::new(provider, model)
    }
}

pub struct Composer {
    provider: Arc<dyn Provider>,
    model: String,
    instructions: String,
    abilities: Vec<Arc<dyn Ability>>,
    output_schema: Option<serde_json::Value>,
    max_turns: Option<u32>,
}

impl Composer {
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            instructions: String::new(),
            abilities: Vec::new(),
            output_schema: None,
            max_turns: None,
        }
    }

    pub fn instructions(mut self, text: impl Into<String>) -> Self {
        self.instructions = text.into();
        self
    }

    pub fn with(mut self, ability: impl Ability + 'static) -> Self {
        self.abilities.push(Arc::new(ability));
        self
    }

    pub fn output_schema(mut self, schema: serde_json::Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    pub fn max_turns(mut self, n: u32) -> Self {
        self.max_turns = Some(n);
        self
    }

    pub async fn build(&self, tenant: &str, session: &str) -> Agent {
        let ctx = BuildCtx {
            tenant,
            session,
            provider: &self.provider,
            model: &self.model,
        };
        let mut comp = Composition::default();
        comp.prompt.instructions(&self.instructions);
        for ability in &self.abilities {
            ability.contribute(&mut comp, &ctx).await;
        }

        let mut b = Agent::builder(self.provider.clone(), tenant, session)
            .model(&self.model)
            .system_prompt(comp.prompt.render());
        for tool in comp.tools {
            b = b.tool(tool);
        }
        for hook in comp.write_hooks {
            b = b.write_hook(hook);
        }
        if let Some(resolver) = comp.media_resolver {
            b = b.media_resolver(resolver);
        }
        if let Some(catalog) = comp.tool_catalog {
            b = b.tool_catalog(catalog);
        }
        if let Some(schema) = &self.output_schema {
            b = b.output_schema(schema.clone());
        }
        if let Some(n) = self.max_turns {
            b = b.max_turns(n);
        }
        b.build()
    }
}

pub struct Skills(pub Arc<SkillSet>);

#[async_trait]
impl Ability for Skills {
    async fn contribute(&self, c: &mut Composition, _ctx: &BuildCtx<'_>) {
        if self.0.is_empty() {
            return;
        }
        c.prompt(Layer::Stable, self.0.prompt_section());
        if let Some(tool) = self.0.view_tool() {
            c.tool(tool);
        }
    }
}

pub struct Delegation {
    roster: Subagents,
    builder: Option<Arc<dyn SubagentBuilder>>,
}

impl Delegation {
    pub fn new(roster: Subagents) -> Self {
        Self {
            roster,
            builder: None,
        }
    }

    pub fn with_builder(mut self, builder: Arc<dyn SubagentBuilder>) -> Self {
        self.builder = Some(builder);
        self
    }
}

#[async_trait]
impl Ability for Delegation {
    async fn contribute(&self, c: &mut Composition, ctx: &BuildCtx<'_>) {
        let builder = self.builder.clone().unwrap_or_else(|| {
            Arc::new(FoundrySubagentBuilder {
                provider: ctx.provider.clone(),
                model: ctx.model.to_string(),
                skills: None,
            })
        });
        c.prompt(Layer::Stable, roster_prompt_section(&self.roster.roster()));
        if let Some(tool) = self.roster.tool(builder) {
            c.tool(tool);
        }
    }
}

pub struct Mcp(pub McpConnection);

#[async_trait]
impl Ability for Mcp {
    async fn contribute(&self, c: &mut Composition, _ctx: &BuildCtx<'_>) {
        if let Some(section) = self.0.section() {
            c.prompt(Layer::Stable, section.to_string());
        }
        if let Some(tool) = self.0.tools() {
            c.tool(tool);
        }
        c.tool_catalog(self.0.catalog());
    }
}

pub struct Sessions(pub SessionsConfig);

#[async_trait]
impl Ability for Sessions {
    async fn contribute(&self, c: &mut Composition, _ctx: &BuildCtx<'_>) {
        if let Some(tool) = self.0.tools() {
            c.tool(tool);
        }
    }
}

pub struct Toolset(pub StdTools);

#[async_trait]
impl Ability for Toolset {
    async fn contribute(&self, c: &mut Composition, _ctx: &BuildCtx<'_>) {
        for tool in self.0.collect() {
            c.tool(tool);
        }
    }
}

pub struct Tools(pub Vec<Arc<dyn Tool>>);

#[async_trait]
impl Ability for Tools {
    async fn contribute(&self, c: &mut Composition, _ctx: &BuildCtx<'_>) {
        for tool in &self.0 {
            c.tool(tool.clone());
        }
    }
}

pub struct Artifacts(pub Arc<dyn ArtifactStore>);

#[async_trait]
impl Ability for Artifacts {
    async fn contribute(&self, c: &mut Composition, ctx: &BuildCtx<'_>) {
        c.media_resolver(Arc::new(ArtifactResolver::new(
            self.0.clone(),
            ctx.tenant,
            ctx.session,
        )));
    }
}

pub struct Memory(pub MemoryConfig);

#[async_trait]
impl Ability for Memory {
    async fn contribute(&self, c: &mut Composition, ctx: &BuildCtx<'_>) {
        let store = self.0.store(ctx.tenant).await;
        if let Ok(snap) = store.snapshot().await {
            c.prompt(Layer::Stable, snap.section(true, true));
        }
        if let Some(tool) = self.0.tools(store.clone()) {
            c.tool(tool);
        }
        if self.0.curation_interval_turns() > 0 {
            let mut hook = MemoryCurator::new(
                self.0.curation_interval_turns(),
                ctx.provider.clone(),
                ctx.model,
                store,
            );
            if let Some(guidance) = self.0.curation_guidance_override() {
                hook = hook.with_guidance(guidance);
            }
            c.write_hook(Arc::new(hook));
        }
    }
}

pub struct Compaction(pub CompactionConfig);

#[async_trait]
impl Ability for Compaction {
    async fn contribute(&self, c: &mut Composition, ctx: &BuildCtx<'_>) {
        c.write_hook(Arc::new(CompactionHook::new(
            &self.0,
            ctx.provider.clone(),
            ctx.model,
        )));
    }
}

pub struct Hooks(pub Vec<Arc<dyn WriteHook>>);

#[async_trait]
impl Ability for Hooks {
    async fn contribute(&self, c: &mut Composition, _ctx: &BuildCtx<'_>) {
        for hook in &self.0 {
            c.write_hook(hook.clone());
        }
    }
}
