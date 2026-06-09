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
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct FilesystemConfig {
    #[serde(default)]
    pub mode: FilesystemMode,

    /// Required iff `mode` is [`FilesystemMode::Isolated`] — the prefix
    /// the sub-agent is rooted at, relative to the parent's storage. The
    /// sub-agent sees this directory AS IF it were `/`.
    #[serde(default)]
    pub root: Option<String>,
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
        match self.mode {
            FilesystemMode::Isolated => {
                let root = self
                    .root
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if root.is_none() {
                    return Err(FilesystemConfigError::IsolatedRequiresRoot);
                }
            }
            FilesystemMode::None | FilesystemMode::Shared => {
                if self.root.is_some() {
                    return Err(FilesystemConfigError::RootRejectedForMode {
                        mode: match self.mode {
                            FilesystemMode::None => "none",
                            FilesystemMode::Shared => "shared",
                            FilesystemMode::Isolated => unreachable!(),
                        },
                    });
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FilesystemConfigError {
    #[error("filesystem.mode=isolated requires a non-empty filesystem.root")]
    IsolatedRequiresRoot,

    #[error("filesystem.root is not allowed with mode={mode} — remove it or change mode to isolated")]
    RootRejectedForMode { mode: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_shared_mode_with_no_root() {
        let cfg = FilesystemConfig::default();
        assert_eq!(cfg.mode, FilesystemMode::Shared);
        assert!(cfg.root.is_none());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn isolated_requires_a_root() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::Isolated,
            root: None,
        };
        assert!(matches!(
            cfg.validate().unwrap_err(),
            FilesystemConfigError::IsolatedRequiresRoot
        ));
    }

    #[test]
    fn isolated_with_empty_string_root_also_rejected() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::Isolated,
            root: Some("   ".into()),
        };
        assert!(matches!(
            cfg.validate().unwrap_err(),
            FilesystemConfigError::IsolatedRequiresRoot
        ));
    }

    #[test]
    fn isolated_with_root_validates() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::Isolated,
            root: Some("wikis".into()),
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn root_rejected_for_shared_mode() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::Shared,
            root: Some("wikis".into()),
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            FilesystemConfigError::RootRejectedForMode { mode: "shared" }
        ));
    }

    #[test]
    fn root_rejected_for_none_mode() {
        let cfg = FilesystemConfig {
            mode: FilesystemMode::None,
            root: Some("wikis".into()),
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            FilesystemConfigError::RootRejectedForMode { mode: "none" }
        ));
    }
}
