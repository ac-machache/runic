//! `SkillsIndexLayer` — the trigger table.
//!
//! Sits in the `CompositeEngine` and renders, every turn, a compact index
//! of every skill in the registry. The agent reads this index, decides
//! whether any skill is relevant, and (if so) calls the `skill_view` tool
//! to load the full body on demand.
//!
//! This is the "name + 1-line description per skill" pattern from hermes
//! and Claude Code — cheap to keep in the system prompt, lets you scale
//! to dozens of skills without inflating context.

use async_trait::async_trait;
use runic_context_engine::{ContextLayer, TurnContext};
use std::sync::Arc;

use crate::SkillRegistry;

pub const DEFAULT_PREAMBLE: &str = "\
You have access to these skills — each one is a focused workflow you can invoke. \
To load a skill's full instructions, call the `skill_view` tool with the skill's `name`. \
For supporting files inside a skill (e.g. references, templates), call `skill_view` \
with both `name` and a `path` relative to the skill's directory.";

pub struct SkillsIndexLayer {
    registry: Arc<SkillRegistry>,
    preamble: String,
}

impl SkillsIndexLayer {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self {
            registry,
            preamble: DEFAULT_PREAMBLE.to_string(),
        }
    }

    /// Override the explanatory preamble. Pass `""` to suppress it entirely
    /// and emit just the index list.
    pub fn with_preamble(mut self, preamble: impl Into<String>) -> Self {
        self.preamble = preamble.into();
        self
    }
}

#[async_trait]
impl ContextLayer for SkillsIndexLayer {
    fn name(&self) -> &str {
        "skills-index"
    }

    async fn render(&self, _ctx: &TurnContext<'_>) -> Option<String> {
        let skills = self.registry.list();
        if skills.is_empty() {
            // No skills → nothing to render. CompositeEngine drops `None`s.
            return None;
        }

        let mut out = String::new();
        out.push_str("<available-skills>\n");
        if !self.preamble.is_empty() {
            out.push_str(&self.preamble);
            out.push_str("\n\n");
        }
        for skill in skills {
            out.push_str(&format!(
                "- {}: {}\n",
                skill.meta.name, skill.meta.description
            ));
        }
        out.push_str("</available-skills>");
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Skill, SkillMeta};
    use runic_message_types::Message;

    fn ctx<'a>() -> TurnContext<'a> {
        TurnContext {
            base_system_prompt: "",
            messages: &[] as &[Message],
            run_id: "r1",
            turn: 0,
            config: runic_context_engine::empty_config(),
        }
    }

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            meta: SkillMeta {
                name: name.into(),
                description: description.into(),
            },
            body: format!("# {name} body"),
            dir: name.into(),
        }
    }

    #[tokio::test]
    async fn empty_registry_renders_none() {
        let registry = Arc::new(SkillRegistry::new());
        let layer = SkillsIndexLayer::new(registry);
        assert!(layer.render(&ctx()).await.is_none());
    }

    #[tokio::test]
    async fn single_skill_renders_index_with_preamble_and_tags() {
        let mut reg = SkillRegistry::new();
        reg.insert(skill("greeter", "says hi"));
        let layer = SkillsIndexLayer::new(Arc::new(reg));

        let out = layer.render(&ctx()).await.expect("should render");
        assert!(out.starts_with("<available-skills>"));
        assert!(out.ends_with("</available-skills>"));
        assert!(out.contains("skill_view"), "preamble should mention the tool");
        assert!(out.contains("- greeter: says hi"));
    }

    #[tokio::test]
    async fn multiple_skills_render_sorted_by_name() {
        let mut reg = SkillRegistry::new();
        reg.insert(skill("zeta", "z"));
        reg.insert(skill("alpha", "a"));
        reg.insert(skill("mu", "m"));
        let layer = SkillsIndexLayer::new(Arc::new(reg));

        let out = layer.render(&ctx()).await.expect("should render");
        let alpha_pos = out.find("alpha").expect("alpha present");
        let mu_pos = out.find("mu").expect("mu present");
        let zeta_pos = out.find("zeta").expect("zeta present");
        assert!(alpha_pos < mu_pos);
        assert!(mu_pos < zeta_pos);
    }

    #[tokio::test]
    async fn empty_preamble_suppresses_the_explanation() {
        let mut reg = SkillRegistry::new();
        reg.insert(skill("foo", "bar"));
        let layer = SkillsIndexLayer::new(Arc::new(reg)).with_preamble("");

        let out = layer.render(&ctx()).await.expect("should render");
        // No preamble text → no mention of skill_view.
        assert!(!out.contains("skill_view"));
        // But the index list is still there.
        assert!(out.contains("- foo: bar"));
    }

    #[tokio::test]
    async fn custom_preamble_replaces_default() {
        let mut reg = SkillRegistry::new();
        reg.insert(skill("foo", "bar"));
        let layer =
            SkillsIndexLayer::new(Arc::new(reg)).with_preamble("Custom preamble text here.");

        let out = layer.render(&ctx()).await.expect("should render");
        assert!(out.contains("Custom preamble text here."));
        assert!(!out.contains("skill_view"));
    }

    #[test]
    fn layer_name_is_stable() {
        let layer = SkillsIndexLayer::new(Arc::new(SkillRegistry::new()));
        assert_eq!(layer.name(), "skills-index");
    }
}
