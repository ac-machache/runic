//! Frontmatter type for `AGENT.md` files.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AgentDef {
    /// Registry-unique name; also what the model sees when calling the
    /// agent as a tool.
    pub name: String,

    /// One-line "when to use this" description — shown to the parent
    /// model so it knows when this sub-agent is relevant.
    pub description: String,

    /// Optional cap on model turns inside the sub-agent. Defaults to 16
    /// at construction time (smaller than the parent's 64). Set lower to
    /// keep a focused sub-agent from running away.
    #[serde(default)]
    pub max_turns: Option<u32>,

    /// Optional key naming which provider this sub-agent runs on, looked
    /// up in the host's keyed provider registry at spawn time (e.g.
    /// `gemini`, `haiku`, `sonnet`, `mistral`). This is how coral runs
    /// `ephy_expert`/`wikis_expert` on Gemini while `crm_expert`/
    /// `purchase_expert` run on Haiku — each sub-agent picks its own
    /// model instead of inheriting the parent's.
    ///
    /// `None` / missing means "inherit the parent's provider". The host
    /// resolves the key; an unknown key is the host's call (fall back to
    /// the parent, or fail at boot). `runic-agents` itself stays
    /// provider-agnostic — it only carries the key.
    #[serde(default)]
    pub provider: Option<String>,

    /// Allowlist of tools (by name) this sub-agent can call. Looked up
    /// in the parent's tool pool at spawn time — names that aren't in
    /// the pool are skipped with a warning. Empty / missing = no tools.
    ///
    /// Use the same name the parent uses (e.g. `search_farms`, or
    /// `mcp__toolbox__search_products` for MCP tools — the `mcp__{server}__`
    /// prefix is part of the canonical name).
    #[serde(default, alias = "tools")]
    pub allowed_tools: Vec<String>,

    /// Allowlist of skills (by name) this sub-agent can access. Loaded
    /// from the parent's skill registry; the sub-agent gets a scoped
    /// `skill_view` tool plus a skills-index block prepended to its
    /// system prompt. Empty / missing = no skills.
    #[serde(default)]
    pub skills: Vec<String>,

    /// Controls the storage surface this sub-agent's shell tools (read_file,
    /// ls, glob, grep, write_file, edit_file) see. See
    /// [`FilesystemConfig`].
    #[serde(default)]
    pub filesystem: FilesystemConfig,

    /// How the parent calls this sub-agent. See [`DispatchMode`].
    #[serde(default)]
    pub dispatch: DispatchMode,
}

/// How the parent agent calls this sub-agent.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DispatchMode {
    /// Parent blocks waiting for the child's response and assembles its
    /// reply in the same turn. Best for short-running children whose
    /// output is directly part of the user-facing answer.
    #[default]
    Sync,

    /// Parent gets a `task_id` back immediately, keeps running, and
    /// polls with `background_status` later. Best for long-running
    /// children when the user shouldn't wait synchronously and the
    /// parent has other useful work it can do in the meantime.
    Async,
}

/// Sub-agent storage scoping for the `runic-shell-tools` surface.
///
/// Default ([`FilesystemMode::Shared`]) reuses the parent's
/// `StorageBackend` — the sub-agent's `read_file` sees the same files
/// the parent does, no scoping. The other two modes change that.
///
/// When `mode: isolated`, EXACTLY ONE of `root` / `path` must be set:
///
/// - `root` is a prefix RELATIVE to the parent's storage — the sub-agent
///   sees that sub-tree as if it were `/`. Use this when the data lives
///   under `~/.runic/...` alongside everything else.
/// - `path` is an ABSOLUTE filesystem path (with leading `~` expanded
///   from `$HOME`) the sub-agent gets a fresh `LocalFsBackend` for. Use
///   this when the data physically lives outside `~/.runic` and you
///   don't want to symlink it in.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct FilesystemConfig {
    #[serde(default)]
    pub mode: FilesystemMode,

    /// Prefix relative to the parent's storage. Mutually exclusive with
    /// `path`. See struct docs.
    #[serde(default)]
    pub root: Option<String>,

    /// Absolute filesystem path. Leading `~/` is expanded from `$HOME`.
    /// Mutually exclusive with `root`. See struct docs.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FilesystemMode {
    /// Shell tools are stripped from the sub-agent's pool. Any names
    /// listed under `allowed-tools` that point at shell tools resolve
    /// to nothing — the model sees an empty seat and can't call them.
    None,

    /// Reuse the parent's storage handle. Sub-agent's `read_file` etc.
    /// see exactly what the parent sees. This is the default.
    #[default]
    Shared,

    /// Mount the sub-agent inside `filesystem.root` of the parent's
    /// storage via [`runic_storage_backend::RootedBackend`]. The
    /// sub-agent addresses files with keys relative to that root.
    Isolated,
}

impl FilesystemConfig {
    /// Per-shape validation called by the registry loader so misconfigs
    /// fail at boot, not mid-session.
    pub fn validate(&self) -> Result<(), FilesystemConfigError> {
        let has_root = self
            .root
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        let has_path = self
            .path
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());

        match self.mode {
            FilesystemMode::Isolated => match (has_root, has_path) {
                (false, false) => Err(FilesystemConfigError::IsolatedRequiresRootOrPath),
                (true, true) => Err(FilesystemConfigError::RootAndPathMutuallyExclusive),
                _ => Ok(()),
            },
            FilesystemMode::None | FilesystemMode::Shared => {
                if has_root || has_path {
                    let mode = match self.mode {
                        FilesystemMode::None => "none",
                        FilesystemMode::Shared => "shared",
                        FilesystemMode::Isolated => unreachable!(),
                    };
                    Err(FilesystemConfigError::RootOrPathRejectedForMode { mode })
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Expand a leading `~/` (or bare `~`) in `path` from `$HOME`. No-op
    /// for absolute paths without a tilde or for `None`. Returns the
    /// raw string if `$HOME` isn't set — caller decides what to do.
    pub fn resolved_path(&self) -> Option<String> {
        let raw = self.path.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            return std::env::var("HOME")
                .ok()
                .map(|home| format!("{home}/{rest}"))
                .or_else(|| Some(raw.to_string()));
        }
        if raw == "~" {
            return std::env::var("HOME").ok().or_else(|| Some(raw.to_string()));
        }
        Some(raw.to_string())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FilesystemConfigError {
    #[error("filesystem.mode=isolated requires exactly one of `root` or `path`")]
    IsolatedRequiresRootOrPath,

    #[error("filesystem.root and filesystem.path are mutually exclusive — pick one")]
    RootAndPathMutuallyExclusive,

    #[error("filesystem.root / filesystem.path are not allowed with mode={mode} — remove them or change mode to isolated")]
    RootOrPathRejectedForMode { mode: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_shared_mode_with_no_root_or_path() {
        let cfg = FilesystemConfig::default();
        assert_eq!(cfg.mode, FilesystemMode::Shared);
        assert!(cfg.root.is_none());
        assert!(cfg.path.is_none());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn isolated_requires_root_or_path() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::Isolated,
            root: None,
            path: None,
        };
        assert!(matches!(
            cfg.validate().unwrap_err(),
            FilesystemConfigError::IsolatedRequiresRootOrPath
        ));
    }

    #[test]
    fn isolated_with_empty_string_root_or_path_rejected() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::Isolated,
            root: Some("   ".into()),
            path: Some("   ".into()),
        };
        assert!(matches!(
            cfg.validate().unwrap_err(),
            FilesystemConfigError::IsolatedRequiresRootOrPath
        ));
    }

    #[test]
    fn isolated_with_root_only_validates() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::Isolated,
            root: Some("wikis".into()),
            path: None,
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn isolated_with_path_only_validates() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::Isolated,
            root: None,
            path: Some("/abs/path".into()),
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn isolated_with_both_root_and_path_rejected() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::Isolated,
            root: Some("wikis".into()),
            path: Some("/abs/path".into()),
        };
        assert!(matches!(
            cfg.validate().unwrap_err(),
            FilesystemConfigError::RootAndPathMutuallyExclusive
        ));
    }

    #[test]
    fn root_or_path_rejected_for_shared_mode() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::Shared,
            root: Some("wikis".into()),
            path: None,
        };
        assert!(matches!(
            cfg.validate().unwrap_err(),
            FilesystemConfigError::RootOrPathRejectedForMode { mode: "shared" }
        ));

        let cfg = FilesystemConfig {
            mode: FilesystemMode::Shared,
            root: None,
            path: Some("/abs".into()),
        };
        assert!(matches!(
            cfg.validate().unwrap_err(),
            FilesystemConfigError::RootOrPathRejectedForMode { mode: "shared" }
        ));
    }

    #[test]
    fn root_or_path_rejected_for_none_mode() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::None,
            root: Some("wikis".into()),
            path: None,
        };
        assert!(matches!(
            cfg.validate().unwrap_err(),
            FilesystemConfigError::RootOrPathRejectedForMode { mode: "none" }
        ));
    }

    #[test]
    fn resolved_path_passes_absolute_through() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::Isolated,
            root: None,
            path: Some("/abs/path/wiki".into()),
        };
        assert_eq!(cfg.resolved_path().unwrap(), "/abs/path/wiki");
    }

    #[test]
    fn resolved_path_expands_tilde() {
        // Save / restore HOME so the test is hermetic. set_var/remove_var
        // are `unsafe` since Rust 2024 — the unsoundness is around
        // concurrent reads in multi-threaded tests; we accept it scoped
        // to this one test (single-threaded by virtue of test runner
        // serialization on env state).
        let saved = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", "/Users/test");
        }
        let cfg = FilesystemConfig {
            mode: FilesystemMode::Isolated,
            root: None,
            path: Some("~/data/wiki".into()),
        };
        assert_eq!(cfg.resolved_path().unwrap(), "/Users/test/data/wiki");
        let cfg2 = FilesystemConfig {
            mode: FilesystemMode::Isolated,
            root: None,
            path: Some("~".into()),
        };
        assert_eq!(cfg2.resolved_path().unwrap(), "/Users/test");
        unsafe {
            match saved {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn resolved_path_is_none_when_unset() {
        let cfg = FilesystemConfig::default();
        assert!(cfg.resolved_path().is_none());
    }
}
