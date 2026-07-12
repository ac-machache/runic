mod calc;
mod composio;
mod hitl;
mod time;
mod weather;
mod web;

use std::sync::Arc;

use runic_tool::Tool;

pub use calc::CalculatorTool;
pub use composio::ComposioTool;
pub use hitl::{AskUserTool, EscalateToHumanTool};
pub use time::SystemTimeTool;
pub use weather::{WeatherHistoryTool, WeatherTool};
pub use web::{
    SearchProvider, SearchResult, SearxngProvider, TavilyProvider, WebFetchTool, WebSearchTool,
};

#[doc(hidden)]
pub use calc::eval as eval_calc;
#[doc(hidden)]
pub use web::{decode_entities, html_to_text};

pub fn default_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(CalculatorTool), Arc::new(SystemTimeTool)]
}
