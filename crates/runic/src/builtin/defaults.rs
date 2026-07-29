use std::sync::Arc;

use runic_tool::Tool;

use super::{CalculatorTool, SystemTimeTool};

pub fn default_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(CalculatorTool), Arc::new(SystemTimeTool)]
}
