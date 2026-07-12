use runic_tool::ToolSpec;

use crate::ability::Layer;

pub struct SkillInfo {
    pub id: String,
    pub description: String,
}

pub struct SubagentInfo {
    pub name: String,
    pub description: String,
}

pub struct AbilityView {
    pub id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub deferred: bool,
    pub activated: bool,
    pub prompt: Vec<(Layer, String)>,
    pub tools: Vec<ToolSpec>,
    pub skills: Vec<SkillInfo>,
    pub subagents: Vec<SubagentInfo>,
    pub hooks: Vec<String>,
}
