//! The `skills(...)` builder — load skills from one path / a `Vec` / a
//! `HashMap`, merge them, and contribute the `skill_view` tool.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use runic_filesystem::Dirs;
use runic_tool::Tool;

use crate::{Skill, SkillRegistry, SkillViewTool};

pub fn skills(dirs: impl Dirs) -> Skills {
    let dirs = dirs.dirs();
    let mut skills: Vec<Skill> = Vec::new();

    for dir in &dirs {
        if !dir.exists() {
            tracing::error!(dir = %dir.display(), "skills dir does not exist — skipping");
            continue;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "skills dir unreadable — skipping");
                continue;
            }
        };

        let (mut loaded, mut dropped) = (0usize, 0usize);
        for entry in entries.flatten() {
            let skill_dir = entry.path();
            if !skill_dir.is_dir() {
                continue;
            }
            let manifest = skill_dir.join("SKILL.md");
            if !manifest.is_file() {
                continue; // a dir without a SKILL.md just isn't a skill
            }
            if load_skill(&manifest, skill_dir, &mut skills) {
                loaded += 1;
            } else {
                dropped += 1;
            }
        }
        tracing::debug!(dir = %dir.display(), loaded, dropped, "scanned skills dir");
    }

    tracing::info!(dirs = dirs.len(), skills = skills.len(), "skills loaded");
    Skills {
        registry: Arc::new(SkillRegistry::new(skills)),
        view_tool: true,
    }
}

/// Read + parse one `SKILL.md`. Returns `true` if it loaded; warns by name and
/// returns `false` if it's unreadable or non-conforming (so it gets dropped).
fn load_skill(manifest: &Path, skill_dir: PathBuf, out: &mut Vec<Skill>) -> bool {
    let text = match std::fs::read_to_string(manifest) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(file = %manifest.display(), error = %e, "cannot read SKILL.md — dropped");
            return false;
        }
    };
    match Skill::parse(skill_dir, &text) {
        Ok(skill) => {
            tracing::debug!(file = %manifest.display(), skill = %skill.name, "loaded skill");
            out.push(skill);
            true
        }
        Err(e) => {
            tracing::warn!(file = %manifest.display(), error = %e, "non-conforming SKILL.md — dropped");
            false
        }
    }
}

pub struct Skills {
    registry: Arc<SkillRegistry>,
    view_tool: bool,
}

impl Skills {
    pub fn include_view_tool(mut self, enabled: bool) -> Self {
        self.view_tool = enabled;
        self
    }
    pub fn registry(&self) -> Arc<SkillRegistry> {
        self.registry.clone()
    }
    pub fn tools(&self) -> Option<Arc<dyn Tool>> {
        if self.view_tool && !self.registry.is_empty() {
            tracing::debug!(count = self.registry.len(), "skill_view tool enabled");
            Some(Arc::new(SkillViewTool::new(self.registry.clone())) as Arc<dyn Tool>)
        } else {
            None
        }
    }
}
