use runic_provider_core::ProviderError;

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
