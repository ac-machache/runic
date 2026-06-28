//! Weather tools — current conditions + history (Open-Meteo, no key).

use std::sync::Arc;

use runic_tool::Tool;
use runic_tools::{WeatherHistoryTool, WeatherTool};

pub fn weather() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(WeatherTool::new()) as Arc<dyn Tool>,
        Arc::new(WeatherHistoryTool::new()),
    ]
}
