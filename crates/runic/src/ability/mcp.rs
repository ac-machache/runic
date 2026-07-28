use std::sync::Arc;

use async_trait::async_trait;
use runic_mcp::McpConnection;

use super::{Ability, BuildCtx, ToAbility};

pub fn direct(connection: impl Into<Arc<McpConnection>>) -> McpDirect {
    McpDirect(connection.into())
}

pub fn deferred(connection: impl Into<Arc<McpConnection>>) -> McpDeferred {
    McpDeferred(connection.into())
}

pub struct McpDirect(Arc<McpConnection>);

#[async_trait]
impl ToAbility for McpDirect {
    fn name(&self) -> &str {
        "mcp-direct"
    }

    async fn to_ability(&self, base: Ability, _ctx: &BuildCtx<'_>) -> anyhow::Result<Ability> {
        Ok(base.tools(self.0.direct_tools()))
    }
}

pub struct McpDeferred(Arc<McpConnection>);

#[async_trait]
impl ToAbility for McpDeferred {
    fn name(&self) -> &str {
        "mcp-deferred"
    }

    async fn to_ability(&self, base: Ability, _ctx: &BuildCtx<'_>) -> anyhow::Result<Ability> {
        let mut base = base;
        if let Some(section) = self.0.section() {
            base = base.prompt(section.to_string());
        }
        if let Some(tool) = self.0.tool_search() {
            base = base.tools([tool]);
        }
        Ok(base.tool_catalog(self.0.catalog()))
    }
}
