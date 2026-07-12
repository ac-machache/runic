use std::sync::Arc;

use async_trait::async_trait;
use runic_hook::WriteHook;
use runic_mcp::McpConnection;
use runic_memory::Memory as MemoryConfig;
use runic_skills::SkillSet;
use runic_subagent::Subagents;
use runic_substrate::Sessions as SessionsConfig;
use runic_tool::Tool;
use runic_tools::Tools as StdTools;

use super::{Ability, AbilityBundle, BuildCtx, Layer};
use crate::hooks::{Compaction as CompactionConfig, CompactionHook, MemoryCurator};

pub struct Skills(pub Arc<SkillSet>);

#[async_trait]
impl Ability for Skills {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        if !self.0.is_empty() {
            bundle.skill_set(self.0.clone());
        }
        Ok(())
    }
}

pub struct Delegation(pub Subagents);

#[async_trait]
impl Ability for Delegation {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        for def in self.0.roster().all() {
            bundle.subagent(def.clone());
        }
        Ok(())
    }
}

pub struct Mcp(pub McpConnection);

#[async_trait]
impl Ability for Mcp {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        if let Some(section) = self.0.section() {
            bundle.prompt(Layer::Stable, section.to_string());
        }
        if let Some(tool) = self.0.tools() {
            bundle.tool(tool);
        }
        bundle.tool_catalog(self.0.catalog());
        Ok(())
    }
}

pub struct Sessions(pub SessionsConfig);

#[async_trait]
impl Ability for Sessions {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        if let Some(tool) = self.0.tools() {
            bundle.tool(tool);
        }
        Ok(())
    }
}

pub struct Toolset(pub StdTools);

#[async_trait]
impl Ability for Toolset {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        for tool in self.0.collect() {
            bundle.tool(tool);
        }
        Ok(())
    }
}

pub struct Tools(pub Vec<Arc<dyn Tool>>);

#[async_trait]
impl Ability for Tools {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        for tool in &self.0 {
            bundle.tool(tool.clone());
        }
        Ok(())
    }
}

pub struct Memory(pub MemoryConfig);

#[async_trait]
impl Ability for Memory {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        let store = self.0.store(ctx.tenant).await;
        if let Ok(snapshot) = store.snapshot().await {
            bundle.prompt(Layer::Stable, snapshot.section(true, true));
        }
        if let Some(tool) = self.0.tools(store.clone()) {
            bundle.tool(tool);
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
            bundle.write_hook(Arc::new(hook));
        }
        Ok(())
    }
}

pub struct Compaction(pub CompactionConfig);

#[async_trait]
impl Ability for Compaction {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        bundle.write_hook(Arc::new(CompactionHook::new(
            &self.0,
            ctx.provider.clone(),
            ctx.model,
        )));
        Ok(())
    }
}

pub struct Hooks(pub Vec<Arc<dyn WriteHook>>);

#[async_trait]
impl Ability for Hooks {
    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        for hook in &self.0 {
            bundle.write_hook(hook.clone());
        }
        Ok(())
    }
}
