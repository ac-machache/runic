//! Types returned by `StorageBackend::list` and `metadata`.

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

/// One entry returned by `StorageBackend::list`.
#[derive(Debug, Clone)]
pub struct Entry {
    pub key: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub modified: Option<DateTime<Utc>>,
}

/// Metadata for a single key.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
}
