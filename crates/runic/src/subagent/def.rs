use crate::composer::Agent;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Invocation {
    #[default]
    Any,
    Sync,
    Background,
}

impl Invocation {
    pub fn as_str(self) -> &'static str {
        match self {
            Invocation::Any => "any",
            Invocation::Sync => "sync",
            Invocation::Background => "background",
        }
    }

    pub fn resolve(self, requested_background: bool) -> bool {
        match self {
            Invocation::Any => requested_background,
            Invocation::Sync => false,
            Invocation::Background => true,
        }
    }
}

#[derive(Clone)]
pub struct Subagent {
    pub name: String,
    pub description: String,
    pub agent: Agent,
    pub invocation: Invocation,
}

impl std::fmt::Debug for Subagent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subagent")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("invocation", &self.invocation)
            .finish_non_exhaustive()
    }
}

impl Subagent {
    pub fn new(name: impl Into<String>, description: impl Into<String>, agent: Agent) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            agent,
            invocation: Invocation::Any,
        }
    }

    pub fn invocation(mut self, invocation: Invocation) -> Self {
        self.invocation = invocation;
        self
    }

    pub fn roster_line(&self) -> String {
        match self.invocation {
            Invocation::Any => format!("- {}: {}", self.name, self.description),
            Invocation::Sync => format!("- {}: {} (runs inline)", self.name, self.description),
            Invocation::Background => format!(
                "- {}: {} (runs in the background — returns a task_id, poll with check_result)",
                self.name, self.description
            ),
        }
    }
}

pub(crate) const DEFAULT_TAG: &str = "subagents";

pub(crate) fn default_intro(tool_name: &str) -> String {
    format!(
        "You can delegate self-contained tasks to these subagents via the \
         `{tool_name}` tool (they do NOT see this conversation):"
    )
}

#[derive(Clone, Default)]
pub struct RosterVoice {
    pub tag: Option<String>,
    pub intro: Option<String>,
    pub tool_name: Option<String>,
    pub tool_description: Option<String>,
}

impl RosterVoice {
    pub fn resolved_tool_name(&self) -> &str {
        self.tool_name
            .as_deref()
            .unwrap_or(crate::subagent::delegate::DEFAULT_TOOL_NAME)
    }

    pub fn roster_section(&self, subagents: &[Subagent]) -> String {
        if subagents.is_empty() {
            return String::new();
        }
        let tag = self.tag.as_deref().unwrap_or(DEFAULT_TAG);
        let intro = match &self.intro {
            Some(text) => text.clone(),
            None => default_intro(self.resolved_tool_name()),
        };
        let lines: Vec<String> = subagents.iter().map(Subagent::roster_line).collect();
        format!("<{tag}>\n{intro}\n{}\n</{tag}>", lines.join("\n"))
    }

    pub fn merge_first_wins(&mut self, other: &RosterVoice) {
        self.tag = self.tag.take().or_else(|| other.tag.clone());
        self.intro = self.intro.take().or_else(|| other.intro.clone());
        self.tool_name = self.tool_name.take().or_else(|| other.tool_name.clone());
        self.tool_description = self
            .tool_description
            .take()
            .or_else(|| other.tool_description.clone());
    }
}

pub fn roster_prompt_section(subagents: &[Subagent]) -> String {
    RosterVoice::default().roster_section(subagents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer::Agent;
    use runic_agent::Llm;
    use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
    use std::sync::Arc;

    struct P;

    #[async_trait::async_trait]
    impl Provider for P {
        async fn complete(
            &self,
            _r: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            Err(ProviderError::Parse("unused".into()))
        }
    }

    fn sub(name: &str, description: &str) -> Subagent {
        Subagent::new(name, description, Agent::new(Llm::new(Arc::new(P), "m")))
    }

    #[test]
    fn roster_line_renders_name_and_description() {
        assert_eq!(
            sub("reviewer", "reviews diffs").roster_line(),
            "- reviewer: reviews diffs"
        );
    }

    #[test]
    fn roster_section_lists_every_subagent_and_is_empty_without_any() {
        assert!(roster_prompt_section(&[]).is_empty());
        let section = roster_prompt_section(&[sub("a", "does A"), sub("b", "does B")]);
        assert!(section.starts_with("<subagents>"));
        assert!(section.contains("- a: does A"));
        assert!(section.contains("- b: does B"));
    }

    #[test]
    fn voice_overrides_tag_and_intro() {
        let voice = RosterVoice {
            tag: Some("crew".into()),
            intro: Some("Your crew:".into()),
            ..Default::default()
        };
        let section = voice.roster_section(&[sub("a", "does A")]);
        assert!(section.starts_with("<crew>"));
        assert!(section.contains("Your crew:"));
        assert!(section.ends_with("</crew>"));
    }
}
