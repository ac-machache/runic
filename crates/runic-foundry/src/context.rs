use runic_memory::BoundedMemoryStore;
use runic_skills::{SkillRegistry, skills_prompt_section};
use runic_subagent::{AgentRoster, roster_prompt_section};

/// Composes the agent's system prompt from each configured part's section, in
/// the order they're added. Empty sections are dropped.
#[derive(Default)]
pub struct Context {
    sections: Vec<String>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn instructions(&mut self, text: &str) -> &mut Self {
        self.push(text);
        self
    }

    pub async fn memory(&mut self, store: &BoundedMemoryStore, mem: bool, user: bool) -> &mut Self {
        if let Ok(snap) = store.snapshot().await {
            self.push(&snap.section(mem, user));
        }
        self
    }

    pub fn skills(&mut self, registry: &SkillRegistry) -> &mut Self {
        if !registry.is_empty() {
            self.push(&skills_prompt_section(registry));
        }
        self
    }

    pub fn subagents(&mut self, roster: &AgentRoster) -> &mut Self {
        if !roster.is_empty() {
            self.push(&roster_prompt_section(roster));
        }
        self
    }

    pub fn mcp(&mut self, section: Option<&str>) -> &mut Self {
        if let Some(s) = section {
            self.push(s);
        }
        self
    }

    fn push(&mut self, s: &str) {
        if !s.trim().is_empty() {
            self.sections.push(s.to_string());
        }
    }

    pub fn render(&self) -> String {
        self.sections.join("\n\n")
    }
}
