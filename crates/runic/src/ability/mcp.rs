use std::sync::Arc;

use async_trait::async_trait;
use runic_mcp::McpConnection;

use super::{Ability, AbilityBundle, BuildCtx, Layer};

pub fn direct(connection: impl Into<Arc<McpConnection>>) -> McpDirect {
    McpDirect(connection.into())
}

pub fn deferred(connection: impl Into<Arc<McpConnection>>) -> McpDeferred {
    McpDeferred(connection.into())
}

pub struct McpDirect(Arc<McpConnection>);

#[async_trait]
impl Ability for McpDirect {
    fn name(&self) -> &str {
        "mcp-direct"
    }

    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        for tool in self.0.direct_tools() {
            bundle.tool(tool);
        }
        Ok(())
    }
}

pub struct McpDeferred(Arc<McpConnection>);

#[async_trait]
impl Ability for McpDeferred {
    fn name(&self) -> &str {
        "mcp-deferred"
    }

    async fn contribute(
        &self,
        bundle: &mut AbilityBundle,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<()> {
        if let Some(section) = self.0.section() {
            bundle.prompt(Layer::Stable, section.to_string());
        }
        if let Some(tool) = self.0.tool_search() {
            bundle.tool(tool);
        }
        bundle.tool_catalog(self.0.catalog());
        Ok(())
    }
}
