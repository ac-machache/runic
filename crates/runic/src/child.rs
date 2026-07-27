use std::sync::Arc;

use crate::subagent::{SubagentBuilder, SubagentReq};
use crate::tools::default_tools;
use async_trait::async_trait;
use runic_provider::Provider;
use runic_skills::SkillSet;
use runic_tool::Tool;

pub struct FoundrySubagentBuilder {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub skills: Option<Arc<SkillSet>>,
}

#[async_trait]
impl SubagentBuilder for FoundrySubagentBuilder {
    async fn provider(&self, _req: &SubagentReq<'_>) -> Arc<dyn Provider> {
        self.provider.clone()
    }

    fn default_model(&self, _req: &SubagentReq<'_>) -> String {
        self.model.clone()
    }

    async fn tool_pool(&self, _req: &SubagentReq<'_>) -> Vec<Arc<dyn Tool>> {
        default_tools()
    }

    fn skill_catalog(&self, _req: &SubagentReq<'_>) -> Option<Arc<SkillSet>> {
        self.skills.clone()
    }
}
