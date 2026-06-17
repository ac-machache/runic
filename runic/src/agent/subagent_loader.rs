use std::path::{Path, PathBuf};
use std::sync::Arc;

use runic_agents::{DispatchMode, FilesystemConfig, FilesystemMode, MdAgent, SubagentSetup};
use runic_provider_core::Provider;
use runic_skills::SkillRegistry;
use runic_storage_backend::{LocalFsBackend, RootedBackend, StorageBackend};
use runic_tool_core::ToolRegistry;

use crate::agent::providers::Providers;
use crate::agent::wiki_tools::{GetImageTool, GetPageContentTool};

pub fn load_subagents(roots: &[PathBuf]) -> Vec<MdAgent> {
    let mut agents = Vec::new();
    for root in roots {
        collect_md(root, &mut agents);
    }
    agents
}

fn collect_md(dir: &Path, out: &mut Vec<MdAgent>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        tracing::debug!(dir = %dir.display(), "subagents scan: unreadable dir, skipping");
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_md(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            match MdAgent::parse(&raw) {
                Ok(mut agent) => {
                    agent.dir = path
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();
                    out.push(agent);
                }
                Err(e) if raw.trim_start().starts_with("---") => {
                    tracing::warn!(file = %path.display(), error = %e, "skipping malformed agent file");
                }
                Err(_) => {}
            }
        }
    }
}

fn backend_for(fs: &FilesystemConfig, parent: &Arc<dyn StorageBackend>) -> Arc<dyn StorageBackend> {
    match fs.mode {
        FilesystemMode::Shared | FilesystemMode::None => parent.clone(),
        FilesystemMode::Isolated => {
            if let Some(path) = fs.resolved_path() {
                Arc::new(LocalFsBackend::new(path))
            } else if let Some(root) = fs.root.as_deref() {
                Arc::new(RootedBackend::new(parent.clone(), root))
            } else {
                parent.clone()
            }
        }
    }
}

pub fn register_subagents(
    agents: &[MdAgent],
    providers: &Providers,
    parent_provider: &Arc<dyn Provider>,
    parent_storage: &Arc<dyn StorageBackend>,
    mcp_pool: &ToolRegistry,
    out: &mut ToolRegistry,
    // Cross-cutting hooks installed on EVERY sub-agent (e.g. the per-run
    // call-limit cap), so a runaway loop is bounded inside children too.
    hooks: &[Arc<dyn runic_agent_core::Hook>],
) {
    let skills = Arc::new(SkillRegistry::new()); // empty for now

    for agent in agents {
        let def = &agent.def;
        let provider = providers.resolve_or(def.provider.as_deref(), parent_provider);
        let backend = backend_for(&def.filesystem, parent_storage);

        // Start from the shared MCP tools, then add filesystem-scoped
        // tools bound to THIS sub-agent's backend. `allowed_tools`
        // filtering inside the factory drops whatever this agent didn't ask for.
        let mut pool = mcp_pool.clone();
        if def.filesystem.mode != FilesystemMode::None {
            runic_shell_tools::register_all(&mut pool, backend.clone());
            pool.register(Arc::new(GetPageContentTool::new(backend.clone())));
            pool.register(Arc::new(GetImageTool::new(backend.clone())));
        }
        let pool = Arc::new(pool);

        match def.dispatch {
            DispatchMode::Sync => {
                let tool = agent.make_subagent_tool_with_context(SubagentSetup {
                    provider,
                    parent_pool: pool,
                    parent_skills: skills.clone(),
                    storage: backend,
                    skills_root: "skills",
                    persister: None,
                    hooks: hooks.to_vec(),
                });
                out.register(Arc::new(tool));
            }
            DispatchMode::Async => {
                let tool = agent.make_async_subagent_tool_with_context(SubagentSetup {
                    provider,
                    parent_pool: pool,
                    parent_skills: skills.clone(),
                    storage: backend,
                    skills_root: "skills",
                    persister: None,
                    hooks: hooks.to_vec(),
                });
                out.register_background(Arc::new(tool));
            }
        }
    }
}
