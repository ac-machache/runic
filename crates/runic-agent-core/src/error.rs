use runic_provider_core::ProviderError;
use runic_tool_core::ToolDispatchError;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),

    #[error("tool '{tool}' not found")]
    UnknownTool { tool: String },

    #[error("tool '{tool}' failed: {message}")]
    ToolFailed { tool: String, message: String },

    #[error("tool input could not be parsed as JSON: {0}")]
    ToolInputParse(String),

    #[error("hook requested stop")]
    HookStop,

    #[error("max turns ({0}) exceeded")]
    MaxTurnsExceeded(u32),

    #[error("internal: {0}")]
    Internal(String),

    #[error("no Approval registred in runtime context (required by HitlTool '{tool}')")]
    ApproverMissing { tool: String },
}

impl From<ToolDispatchError> for AgentError {
    fn from(err: ToolDispatchError) -> Self {
        match err {
            ToolDispatchError::UnknownTool { tool } => AgentError::UnknownTool { tool },
        }
    }
}
