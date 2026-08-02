use std::sync::Arc;

use runic_macros::tool;
use runic_tool::{ToolContext, ToolResult};

use crate::set::SkillSet;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ReadSkillArgs {
    #[schemars(description = "Skill id from the index (e.g. `core:deploy`).")]
    name: String,
    #[serde(default)]
    #[schemars(description = "Optional file path relative to the skill folder.")]
    path: Option<String>,
}

#[tool(
    name = "read_skill",
    args = ReadSkillArgs,
    description = "Read a skill's full instructions by `name`, or a file inside the skill's \
                   folder by also passing a relative `path`."
)]
pub struct ReadSkillTool {
    set: Arc<SkillSet>,
}

impl ReadSkillTool {
    pub fn new(set: Arc<SkillSet>) -> Self {
        Self { set }
    }

    async fn tool(&self, args: ReadSkillArgs, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let Some(skill) = self.set.get(&args.name) else {
            return Ok(ToolResult::error(format!("unknown skill '{}'", args.name)));
        };

        match args.path.as_deref() {
            None => Ok(ToolResult::ok(skill.body.clone())),
            Some(rel) => match self.set.read_subfile(skill, rel).await {
                Ok(content) => Ok(ToolResult::ok(content)),
                Err(error) => Ok(ToolResult::error(error.to_string())),
            },
        }
    }
}
