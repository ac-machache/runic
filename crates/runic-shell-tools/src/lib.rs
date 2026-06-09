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

use std::sync::Arc;

use runic_storage_backend::StorageBackend;
use runic_tool_core::ToolRegistry;

/// Canonical name every tool in this crate emits via `Tool::name()`.
/// Use this when you need to strip the shell-tool surface from a
/// pre-built `ToolRegistry` (e.g. a sub-agent whose `filesystem.mode`
/// is `none`) or re-register the same set against a different
/// `StorageBackend` (e.g. `filesystem.mode: isolated`).
pub const SHELL_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "ls",
    "glob",
    "grep",
];

/// Register all six shell tools into `registry`, bound to `storage`.
///
/// If `registry` already contains entries under these names, they are
/// **overwritten** — exactly the behaviour the sub-agent isolation flow
/// wants: clone the parent pool, then call `register_all` against the
/// child's namespaced backend to swap the shell-tool dispatches in place.
pub fn register_all(registry: &mut ToolRegistry, storage: Arc<dyn StorageBackend>) {
    registry.register(Arc::new(ReadFileTool::new(storage.clone())));
    registry.register(Arc::new(WriteFileTool::new(storage.clone())));
    registry.register(Arc::new(EditFileTool::new(storage.clone())));
    registry.register(Arc::new(LsTool::new(storage.clone())));
    registry.register(Arc::new(GlobTool::new(storage.clone())));
    registry.register(Arc::new(GrepTool::new(storage)));
}

/// Remove every shell tool from `registry`. Used when a sub-agent
/// declares `filesystem.mode: none` — even if a misconfigured
/// `allowed-tools` listed shell tools by name, they end up unreachable
/// because they're no longer in the pool.
pub fn deregister_all(registry: &mut ToolRegistry) {
    for name in SHELL_TOOL_NAMES {
        registry.remove(name);
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;
    use runic_storage_backend::MemoryBackend;

    #[test]
    fn shell_tool_names_matches_what_each_tool_reports() {
        // Sanity: keep the const aligned with what each tool actually
        // calls itself, so the strip / re-register flows can't drift.
        use runic_tool_core::Tool;
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        assert_eq!(ReadFileTool::new(storage.clone()).name(), "read_file");
        assert_eq!(WriteFileTool::new(storage.clone()).name(), "write_file");
        assert_eq!(EditFileTool::new(storage.clone()).name(), "edit_file");
        assert_eq!(LsTool::new(storage.clone()).name(), "ls");
        assert_eq!(GlobTool::new(storage.clone()).name(), "glob");
        assert_eq!(GrepTool::new(storage).name(), "grep");
        assert_eq!(
            SHELL_TOOL_NAMES,
            &["read_file", "write_file", "edit_file", "ls", "glob", "grep"]
        );
    }

    #[test]
    fn register_all_puts_all_six_into_an_empty_registry() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let mut reg = ToolRegistry::new();
        register_all(&mut reg, storage);
        assert_eq!(reg.len(), 6);
        for name in SHELL_TOOL_NAMES {
            assert!(reg.get(name).is_some(), "missing {name}");
        }
    }

    #[test]
    fn deregister_all_strips_every_shell_tool() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let mut reg = ToolRegistry::new();
        register_all(&mut reg, storage);
        deregister_all(&mut reg);
        for name in SHELL_TOOL_NAMES {
            assert!(reg.get(name).is_none(), "{name} still present");
        }
        assert_eq!(reg.len(), 0);
    }
}
