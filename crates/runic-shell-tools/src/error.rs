//! Error type shared by every shell tool.

#[derive(Debug, thiserror::Error)]
pub enum ShellToolError {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("missing required field: {field}")]
    MissingField { field: &'static str },

    #[error("invalid path {path:?}: {reason}")]
    InvalidPath { path: String, reason: &'static str },

    #[error("invalid glob pattern {pattern:?}: {source}")]
    InvalidGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },

    #[error("invalid regex {pattern:?}: {source}")]
    InvalidRegex {
        pattern: String,
        #[source]
        source: regex::Error,
    },

    #[error("input is not UTF-8: {0}")]
    NotUtf8(String),

    #[error("no entry contained the old_string (and replace_all is false)")]
    NoMatch,

    #[error("old_string matched {count} times — pass replace_all=true to apply to all of them")]
    Ambiguous { count: usize },

    #[error("write of {actual} bytes exceeds the cap of {limit}")]
    OverWriteCap { actual: usize, limit: usize },

    #[error("invalid value for {field}: {reason}")]
    InvalidValue { field: &'static str, reason: String },
}
