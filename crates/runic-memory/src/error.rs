//! Error type for memory store and tool.

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("would exceed cap for {target}: {actual}/{limit} chars (remove or replace an entry first)")]
    OverLimit {
        target: String,
        actual: usize,
        limit: usize,
    },

    #[error("entry too long: {actual} chars > per-target cap of {limit}")]
    EntryTooLong { actual: usize, limit: usize },

    #[error("no entry contained substring {search:?}")]
    NoMatch { search: String },

    #[error("ambiguous: substring {search:?} matched {count} entries — narrow it")]
    Ambiguous { search: String, count: usize },

    #[error("invalid action {action:?}: must be one of add | replace | remove | read")]
    InvalidAction { action: String },

    #[error("invalid target {target:?}: must be 'memory' or 'user'")]
    InvalidTarget { target: String },

    #[error("missing required field: {field}")]
    MissingField { field: &'static str },

    #[error("blocked by threat scanner ({kind}{}): rephrase the entry", if .detail.is_empty() { String::new() } else { format!(": {}", .detail) })]
    Threat {
        kind: &'static str,
        detail: String,
    },

    #[error(
        "refusing to write {target}: on-disk content was edited externally and would be lost on overwrite. \
         A backup was saved to {backup_key:?}. Inspect it, then re-issue the write."
    )]
    DriftDetected {
        target: String,
        backup_key: String,
    },

    #[error("lock acquisition failed for {target}: {source}")]
    Lock {
        target: String,
        #[source]
        source: std::io::Error,
    },
}
