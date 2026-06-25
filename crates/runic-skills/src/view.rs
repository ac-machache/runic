//! `skill_view` — the tool the model calls to load a skill's body, or a
//! sub-file inside the skill's folder, on demand (progressive disclosure).

use std::sync::Arc;

use async_trait::async_trait;
use runic_tool::{Tool, ToolContext, ToolResult};

use crate::set::SkillSet;

/// Loads a skill's full instructions by `name`, or a file inside the skill's
/// folder by also passing a relative `path`. Reads go through the skill's own
/// [`SkillSource`](crate::SkillSource), so a sub-file of an S3 skill is fetched
/// from S3 — the tool never knows where the skill lives.
pub struct SkillViewTool {
    set: Arc<SkillSet>,
}

impl SkillViewTool {
    pub fn new(set: Arc<SkillSet>) -> Self {
        Self { set }
    }
}

#[async_trait]
impl Tool for SkillViewTool {
    fn name(&self) -> &str {
        "skill_view"
    }

    fn description(&self) -> &str {
        "Load a skill's full instructions by `name`, or a file inside the \
         skill's folder by also passing a relative `path`."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill id from the index (e.g. `core:deploy`)." },
                "path": { "type": "string", "description": "Optional file path relative to the skill folder." }
            },
            "required": ["name"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("skill_view requires `name`"));
        };
        let Some(skill) = self.set.get(name) else {
            return Ok(ToolResult::error(format!("unknown skill '{name}'")));
        };

        match args.get("path").and_then(|v| v.as_str()) {
            None => Ok(ToolResult::ok(skill.body.clone())),
            Some(rel) => match self.set.read_subfile(skill, rel).await {
                Ok(content) => Ok(ToolResult::ok(content)),
                Err(e) => Ok(ToolResult::error(e.to_string())),
            },
        }
    }
}
