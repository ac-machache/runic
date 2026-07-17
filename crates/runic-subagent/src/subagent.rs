use std::sync::Arc;

use runic_tool::Tool;

#[derive(Debug, Clone)]
pub struct Subagent {
    pub name: String,
    pub description: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub allowed_tools: Vec<String>,
    pub skills: Vec<String>,
    pub max_turns: Option<u32>,
    pub system_prompt: String,
}

impl Subagent {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            provider: None,
            model: None,
            allowed_tools: Vec::new(),
            skills: Vec::new(),
            max_turns: None,
            system_prompt: String::new(),
        }
    }

    pub fn prompt(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        if self.system_prompt.is_empty() {
            self.system_prompt = text;
        } else {
            self.system_prompt = format!("{}\n\n{text}", self.system_prompt);
        }
        self
    }

    pub fn provider(mut self, name: impl Into<String>) -> Self {
        self.provider = Some(name.into());
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn allowed_tools(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_tools.extend(names.into_iter().map(Into::into));
        self
    }

    pub fn skills(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.skills.extend(names.into_iter().map(Into::into));
        self
    }

    pub fn max_turns(mut self, turns: u32) -> Self {
        self.max_turns = Some(turns);
        self
    }

    pub fn roster_line(&self) -> String {
        format!("- {}: {}", self.name, self.description)
    }

    pub fn scope_tools(&self, pool: &[Arc<dyn Tool>]) -> Vec<Arc<dyn Tool>> {
        pool.iter()
            .filter(|t| self.allowed_tools.iter().any(|p| glob_matches(p, t.name())))
            .cloned()
            .collect()
    }
}

fn glob_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

pub fn roster_prompt_section(subagents: &[Subagent]) -> String {
    if subagents.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = subagents.iter().map(Subagent::roster_line).collect();
    format!(
        "<subagents>\nYou can delegate self-contained tasks to these subagents \
         via the `delegate` tool (they do NOT see this conversation):\n{}\n</subagents>",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fluent_construction_covers_every_field() {
        let sub = Subagent::new("reviewer", "reviews diffs")
            .prompt("You are a reviewer.")
            .prompt("Be terse.")
            .provider("haiku")
            .model("small")
            .allowed_tools(["read_file", "grep"])
            .skills(["*"])
            .max_turns(8);
        assert_eq!(sub.name, "reviewer");
        assert_eq!(sub.description, "reviews diffs");
        assert_eq!(sub.system_prompt, "You are a reviewer.\n\nBe terse.");
        assert_eq!(sub.provider.as_deref(), Some("haiku"));
        assert_eq!(sub.model.as_deref(), Some("small"));
        assert_eq!(sub.allowed_tools, vec!["read_file", "grep"]);
        assert_eq!(sub.skills, vec!["*"]);
        assert_eq!(sub.max_turns, Some(8));
        assert_eq!(sub.roster_line(), "- reviewer: reviews diffs");
    }

    #[test]
    fn roster_section_lists_every_subagent_and_is_empty_without_any() {
        assert!(roster_prompt_section(&[]).is_empty());
        let section =
            roster_prompt_section(&[Subagent::new("a", "does A"), Subagent::new("b", "does B")]);
        assert!(section.starts_with("<subagents>"));
        assert!(section.contains("- a: does A"));
        assert!(section.contains("- b: does B"));
    }

    use async_trait::async_trait;
    use runic_tool::{ToolContext, ToolResult};

    struct T(&'static str);
    #[async_trait]
    impl Tool for T {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "t"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _a: serde_json::Value,
            _c: &ToolContext,
        ) -> anyhow::Result<ToolResult> {
            Ok(ToolResult::ok(""))
        }
    }

    fn tool_pool(names: &[&'static str]) -> Vec<Arc<dyn Tool>> {
        names
            .iter()
            .map(|&n| Arc::new(T(n)) as Arc<dyn Tool>)
            .collect()
    }

    fn scoped(allowed: &[&str], pool: &[Arc<dyn Tool>]) -> Vec<String> {
        Subagent::new("x", "d")
            .allowed_tools(allowed.iter().copied())
            .scope_tools(pool)
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    }

    #[test]
    fn scope_tools_matches_exact_names_and_drops_unknowns() {
        let pool = tool_pool(&["a", "b", "mcp__crm__search"]);
        assert_eq!(scoped(&["a", "ghost"], &pool), vec!["a"]);
    }

    #[test]
    fn scope_tools_empty_grants_nothing_and_star_grants_the_whole_pool() {
        let pool = tool_pool(&["a", "b", "mcp__crm__search"]);
        assert!(scoped(&[], &pool).is_empty());
        assert_eq!(scoped(&["*"], &pool).len(), 3);
    }

    #[test]
    fn scope_tools_prefix_glob_is_bounded_to_its_namespace() {
        let pool = tool_pool(&[
            "mcp__crm__search",
            "mcp__crm__update",
            "mcp__crmx__thing",
            "mcp__crm_admin__wipe",
        ]);
        assert_eq!(
            scoped(&["mcp__crm__*"], &pool),
            vec!["mcp__crm__search", "mcp__crm__update"]
        );
    }

    #[test]
    fn scope_tools_only_a_trailing_star_is_a_wildcard() {
        let pool = tool_pool(&["mcp__crm__search", "mcp__docs__read"]);
        assert!(scoped(&["mcp__*__search"], &pool).is_empty());
    }

    #[test]
    fn scope_tools_overlapping_patterns_do_not_duplicate() {
        let pool = tool_pool(&["a", "mcp__crm__search"]);
        assert_eq!(
            scoped(&["*", "mcp__crm__*", "a"], &pool),
            vec!["a", "mcp__crm__search"]
        );
    }

    #[test]
    fn scope_tools_never_grants_a_tool_outside_the_pool() {
        let pool = tool_pool(&["a", "b"]);
        assert_eq!(scoped(&["c", "mcp__ghost__*", "*"], &pool), vec!["a", "b"]);
    }
}
