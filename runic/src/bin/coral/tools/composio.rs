//! Composio tool — third-party app actions, enabled when `COMPOSIO_API_KEY`
//! is set. `COMPOSIO_ENTITY_ID` scopes per-user OAuth connections.

use std::sync::Arc;

use runic_tool::Tool;
use runic_tools::ComposioTool;

pub fn composio() -> Vec<Arc<dyn Tool>> {
    match std::env::var("COMPOSIO_API_KEY").ok().filter(|s| !s.is_empty()) {
        Some(key) => {
            let entity = std::env::var("COMPOSIO_ENTITY_ID").ok().filter(|s| !s.is_empty());
            vec![Arc::new(ComposioTool::new(key, entity)) as Arc<dyn Tool>]
        }
        None => Vec::new(),
    }
}
