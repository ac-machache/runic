use async_trait::async_trait;
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_state::AgentState;
use runic_types::ToolCall;

pub struct ForwardContext {
    keys: Vec<String>,
    prefix: String,
}

impl ForwardContext {
    pub fn new(keys: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            keys: keys.into_iter().map(Into::into).collect(),
            prefix: "mcp__".to_string(),
        }
    }

    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }
}

#[async_trait]
impl WriteHook for ForwardContext {
    fn name(&self) -> &str {
        "forward-context"
    }

    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::BeforeTool]
    }

    async fn before_tool(&self, state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        if !call.name.starts_with(&self.prefix) {
            return HookOutcome::Noop;
        }
        let mut values = Vec::with_capacity(self.keys.len());
        for key in &self.keys {
            match state.config.get(key) {
                Some(value) => values.push((key.clone(), value.clone())),
                None => {
                    return HookOutcome::Cancel(format!(
                        "tool `{}` requires run-config key `{key}` (forward_context), which is not set for this run",
                        call.name
                    ));
                }
            }
        }
        if !call.input.is_object() {
            call.input = serde_json::Value::Object(serde_json::Map::new());
        }
        let Some(input) = call.input.as_object_mut() else {
            return HookOutcome::Noop;
        };
        for (key, value) in values {
            input.insert(key, value);
        }
        HookOutcome::Continue
    }
}
