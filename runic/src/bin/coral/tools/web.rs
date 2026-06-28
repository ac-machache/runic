//! Web tools — `web_fetch` (always) plus `web_search` backed by Tavily when
//! `TAVILY_API_KEY` is set.

use std::sync::Arc;

use runic_tool::Tool;
use runic_tools::{TavilyProvider, WebFetchTool, WebSearchTool};

pub fn web() -> Vec<Arc<dyn Tool>> {
    let mut out: Vec<Arc<dyn Tool>> = vec![Arc::new(WebFetchTool::new())];
    if let Some(key) = std::env::var("TAVILY_API_KEY").ok().filter(|s| !s.is_empty()) {
        out.push(Arc::new(WebSearchTool::new(Arc::new(TavilyProvider::new(key)))));
    }
    out
}
