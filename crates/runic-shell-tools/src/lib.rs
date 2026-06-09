//! `runic-shell-tools` — file-system tools backed by a [`StorageBackend`].
//!
//! Six tools an agent can read and mutate the storage tree with:
//!
//! | Tool          | Purpose                                                       |
//! |---------------|---------------------------------------------------------------|
//! | [`ReadFileTool`]   | Read a file, with line-based `offset` + `limit` pagination |
//! | [`WriteFileTool`]  | Overwrite a file end-to-end                                |
//! | [`EditFileTool`]   | String find/replace with unique-match guard                |
//! | [`LsTool`]         | List one directory level                                   |
//! | [`GlobTool`]       | Recursive pattern match (e.g. `**/*.md`)                   |
//! | [`GrepTool`]       | Recursive regex content search with output modes           |
//!
//! ## Why not raw `tokio::fs`?
//!
//! Every tool here takes `Arc<dyn StorageBackend>`. The backend is the
//! sandbox — `LocalFsBackend::new(root)` ties the tool to `root`,
//! `NamespacedBackend::new(inner, "wikis")` scopes it further, and
//! `MemoryBackend` makes tests trivial. There's no path-traversal arms
//! race here because the agent can only ever address what the backend
//! already exposes; the keys aren't OS paths, they're abstract storage
//! keys the backend interprets.
//!
//! ## Caps
//!
//! Every tool has reasonable defaults to keep tool results from blowing
//! the model's context window. Each `with_*` builder method overrides
//! one cap; missing builders means the default applies. See each tool's
//! constants for what they are.
//!
//! ## Registration
//!
//! Each tool is just a `Tool`. Register them via the parent's builder
//! and add them to your sub-agent pool — `allowed-tools: [read_file, …]`
//! in `AGENT.md` is the discovery surface.

pub mod edit_file;
pub mod error;
pub mod glob;
pub mod grep;
pub mod ls;
pub mod paths;
pub mod read_file;
pub mod walk;
pub mod write_file;

pub use edit_file::EditFileTool;
pub use error::ShellToolError;
pub use glob::GlobTool;
pub use grep::{GrepOutputMode, GrepTool};
pub use ls::LsTool;
pub use read_file::ReadFileTool;
pub use write_file::WriteFileTool;
