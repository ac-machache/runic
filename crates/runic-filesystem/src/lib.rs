//! `runic-filesystem` — the agent's filesystem *interface* (the deepagents
//! backend model, in Rust).
//!
//! A [`FilesystemBackend`] is the full set of file operations the agent sees —
//! `ls / read / write / edit / grep / glob` — over a `/`-rooted virtual path
//! space. The *tools* an agent calls are thin adapters over this trait (a
//! separate layer); concrete storage (local disk, in-memory, S3, GCS, …) are
//! `impl FilesystemBackend` living in their own crates.
//!
//! [`CompositeBackend`] is the payoff: mount different backends at path
//! prefixes and the agent sees **one** filesystem — reads, writes, and
//! searches span every mount, with a default for everything else.
//!
//! This crate is deliberately just the **interface + composite** (no concrete
//! storage backend), so you can add your own.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod composite;
pub mod local;
pub mod memory;
pub use composite::CompositeBackend;
pub use local::LocalFs;
pub use memory::MemoryFs;

/// One directory entry / file listing record. Only `path` is guaranteed; the
/// rest are best-effort per backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<DateTime<Utc>>,
}

impl FileInfo {
    pub fn file(path: impl Into<String>, size: u64) -> Self {
        Self {
            path: path.into(),
            is_dir: false,
            size,
            modified_at: None,
        }
    }
    pub fn dir(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            is_dir: true,
            size: 0,
            modified_at: None,
        }
    }
}

/// A single grep hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatch {
    pub path: String,
    /// 1-indexed line number.
    pub line: u32,
    pub text: String,
}

/// The result of a paginated [`FilesystemBackend::read`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResult {
    /// The selected lines, joined.
    pub content: String,
    /// 1-indexed line number of the first returned line.
    pub start_line: usize,
    /// Whether more lines existed past the returned window.
    pub truncated: bool,
}

/// Filesystem errors — normalized so tools can surface recoverable conditions.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum FsError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("is a directory: {0}")]
    IsDirectory(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("the search string was not found")]
    NoEditMatch,
    #[error("found {0} occurrences — pass replace_all or use a unique string")]
    AmbiguousEdit(usize),
    #[error("io error: {0}")]
    Io(String),
    #[error("backend error: {0}")]
    Backend(String),
}

/// The agent-facing filesystem. Implement this for any storage location; the
/// fs tools and [`CompositeBackend`] work over it uniformly.
///
/// **Path convention:** every path is absolute and `/`-rooted. A backend
/// interprets paths under its own root; [`CompositeBackend`] strips a mount's
/// prefix before delegating and re-prepends it on the way out, so the agent
/// always sees one unified namespace.
#[async_trait]
pub trait FilesystemBackend: Send + Sync {
    /// List a directory (one level). For `"/"`, the backend's root.
    async fn ls(&self, path: &str) -> Result<Vec<FileInfo>, FsError>;

    /// Read a file, paginated by line (`offset` 0-indexed, `limit` lines).
    async fn read(&self, path: &str, offset: usize, limit: usize) -> Result<ReadResult, FsError>;

    /// Create a new file. Errors with [`FsError::AlreadyExists`] if present.
    async fn write(&self, path: &str, content: &str) -> Result<(), FsError>;

    /// Exact string replacement. Without `replace_all`, `old` must be unique
    /// (else [`FsError::AmbiguousEdit`]); returns the number of replacements.
    async fn edit(
        &self,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize, FsError>;

    /// Literal-substring search (NOT regex). `path` scopes the search (`None`
    /// = whole backend); `glob` filters which files by name.
    async fn grep(
        &self,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>, FsError>;

    /// Find files matching a glob pattern, optionally under `path`.
    async fn glob(&self, pattern: &str, path: Option<&str>) -> Result<Vec<FileInfo>, FsError>;

    /// Delete a file. Errors with [`FsError::NotFound`] if absent, or
    /// [`FsError::IsDirectory`] for a directory.
    async fn delete(&self, path: &str) -> Result<(), FsError>;
}
