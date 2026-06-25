use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use runic_agent::Agent;
use runic_filesystem::FilesystemBackend;
use runic_provider::Provider;
use runic_subagent::{AgentDef, AgentRoster, ChildBuilder, DelegateTool, DelegationCtx};
use runic_tool::Tool;
use runic_tools::default_tools;

use crate::dirs::Dirs;

pub fn subagents(dirs: impl Dirs) -> Subagents {
    let dirs = dirs.dirs();
    let mut defs: Vec<AgentDef> = Vec::new();

    for dir in &dirs {
        if !dir.exists() {
            tracing::error!(dir = %dir.display(), "subagents dir does not exist — skipping");
            continue;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "subagents dir unreadable — skipping");
                continue;
            }
        };

        let (mut loaded, mut dropped) = (0usize, 0usize);
        for entry in entries.flatten() {
            let path = entry.path();
            let file = if path.is_dir() {
                let md = path.join("AGENT.md");
                md.is_file().then_some(md)
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                Some(path)
            } else {
                None
            };
            if let Some(file) = file {
                if load_agent(&file, &mut defs) {
                    loaded += 1;
                } else {
                    dropped += 1;
                }
            }
        }
        tracing::debug!(dir = %dir.display(), loaded, dropped, "scanned subagents dir");
    }

    tracing::info!(
        dirs = dirs.len(),
        subagents = defs.len(),
        "subagents loaded"
    );
    Subagents {
        roster: Arc::new(AgentRoster::new(defs)),
    }
}

/// Read + parse one agent file. Returns `true` if it loaded; warns by name and
/// returns `false` if it's unreadable or non-conforming (so it gets dropped).
fn load_agent(file: &Path, defs: &mut Vec<AgentDef>) -> bool {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(file = %file.display(), error = %e, "cannot read agent file — dropped");
            return false;
        }
    };
    match AgentDef::parse_markdown(&text) {
        Ok(def) => {
            tracing::debug!(file = %file.display(), agent = %def.name, "loaded subagent");
            defs.push(def);
            true
        }
        Err(e) => {
            tracing::warn!(file = %file.display(), error = %e, "non-conforming AGENT.md — dropped");
            false
        }
    }
}

pub struct Subagents {
    roster: Arc<AgentRoster>,
}

impl Subagents {
    pub fn roster(&self) -> Arc<AgentRoster> {
        self.roster.clone()
    }

    pub fn tools(
        &self,
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        fs: Arc<dyn FilesystemBackend>,
    ) -> Option<Arc<dyn Tool>> {
        if self.roster.is_empty() {
            return None;
        }
        let child: Arc<dyn ChildBuilder> = Arc::new(FoundryChildBuilder {
            provider,
            model: model.into(),
            fs,
        });
        tracing::debug!(count = self.roster.len(), "delegate tool enabled");
        Some(Arc::new(DelegateTool::new(self.roster.clone(), child)) as Arc<dyn Tool>)
    }
}

struct FoundryChildBuilder {
    provider: Arc<dyn Provider>,
    model: String,
    fs: Arc<dyn FilesystemBackend>,
}

#[async_trait]
impl ChildBuilder for FoundryChildBuilder {
    async fn build(&self, def: &AgentDef, _dctx: &DelegationCtx) -> anyhow::Result<Agent> {
        let model = def.model.clone().unwrap_or_else(|| self.model.clone());
        let mut b = Agent::builder(self.provider.clone(), "subagent", &def.name)
            .model(model)
            .system_prompt(&def.system_prompt);
        for t in default_tools(self.fs.clone()) {
            b = b.tool(t);
        }
        if let Some(max_turns) = def.max_turns {
            b = b.max_turns(max_turns);
        }
        Ok(b.build())
    }
}
