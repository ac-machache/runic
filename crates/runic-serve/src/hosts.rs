use crate::error::ServeError;
use std::collections::HashMap;

#[derive(Clone)]
pub struct HostedAgents {
    pub agent: runic::Agent,
    pub description: Option<String>,
}

impl HostedAgents {
    pub fn new(agent: runic::Agent) -> Self {
        Self {
            agent,
            description: None,
        }
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

impl From<runic::Agent> for HostedAgents {
    fn from(agent: runic::Agent) -> Self {
        Self::new(agent)
    }
}

pub struct AgentRegistry {
    hosted: HashMap<String, HostedAgents>,
}

impl AgentRegistry {
    pub fn new(hosted: HashMap<String, HostedAgents>) -> Self {
        assert!(
            !hosted.is_empty(),
            "runic-serve needs at least one agent registered"
        );
        Self { hosted }
    }
    pub fn get(&self, agent: &str) -> Result<&HostedAgents, ServeError> {
        self.hosted
            .get(agent)
            .ok_or_else(|| ServeError::AgentNotFound {
                name: agent.to_string(),
            })
    }

    pub fn resolve_agent(&self, requested: Option<&str>) -> Result<String, ServeError> {
        match requested {
            Some(name) => {
                self.get(name)?;
                Ok(name.to_string())
            }
            None if self.hosted.len() == 1 => Ok(self.hosted.keys().next().unwrap().clone()),
            None => {
                let mut names: Vec<_> = self.hosted.keys().map(String::as_str).collect();
                names.sort_unstable();
                Err(ServeError::BadRequest(format!(
                    "this server hosts several agents; set \"agent\" to one of: {}",
                    names.join(", ")
                )))
            }
        }
    }

    pub fn agent_names(&self) -> Vec<(&str, Option<&str>)> {
        let mut names: Vec<_> = self
            .hosted
            .iter()
            .map(|(name, hosted)| (name.as_str(), hosted.description.as_deref()))
            .collect();
        names.sort_by_key(|(name, _)| *name);
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use runic::types::{ContentBlock, StopReason, TokenUsage};
    use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
    use std::sync::Arc;

    struct TestProvider;

    #[async_trait]
    impl Provider for TestProvider {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "ok".into(),
                    provider_metadata: None,
                }],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: TokenUsage::default(),
            })
        }
    }

    fn agent() -> runic::Agent {
        runic::Agent::new(
            runic::Llm::new(Arc::new(TestProvider), "test-model").instructions("test"),
        )
    }

    fn registry(names: &[&str]) -> AgentRegistry {
        AgentRegistry::new(
            names
                .iter()
                .map(|name| (name.to_string(), HostedAgents::new(agent())))
                .collect(),
        )
    }

    #[test]
    fn a_lone_agent_answers_when_the_client_names_nobody() {
        let solo = registry(&["solo"]);
        assert_eq!(solo.resolve_agent(None).unwrap(), "solo");
        assert_eq!(solo.resolve_agent(Some("solo")).unwrap(), "solo");
    }

    #[test]
    fn several_agents_make_the_client_choose() {
        let pair = registry(&["coral", "scout"]);
        match pair.resolve_agent(None) {
            Err(ServeError::BadRequest(message)) => {
                assert!(
                    message.contains("coral") && message.contains("scout"),
                    "the error should list what to pick from, got {message:?}"
                );
            }
            other => panic!("expected a bad request, got {other:?}"),
        }
        assert_eq!(pair.resolve_agent(Some("scout")).unwrap(), "scout");
    }

    #[test]
    fn an_unknown_name_is_not_found_however_many_are_hosted() {
        for hosted in [registry(&["solo"]), registry(&["coral", "scout"])] {
            assert!(matches!(
                hosted.resolve_agent(Some("ghost")),
                Err(ServeError::AgentNotFound { .. })
            ));
            assert!(matches!(
                hosted.get("ghost"),
                Err(ServeError::AgentNotFound { .. })
            ));
        }
    }

    #[test]
    fn names_come_back_sorted_with_their_descriptions() {
        let hosted = AgentRegistry::new(HashMap::from([
            ("scout".to_string(), HostedAgents::new(agent())),
            (
                "coral".to_string(),
                HostedAgents::new(agent()).describe("the main one"),
            ),
        ]));
        assert_eq!(
            hosted.agent_names(),
            vec![("coral", Some("the main one")), ("scout", None)]
        );
    }

    #[test]
    fn a_hosted_agent_keeps_the_agent_it_was_built_from() {
        let hosted = HostedAgents::from(agent());
        assert_eq!(hosted.agent.model(), "test-model");
        assert!(hosted.description.is_none());
    }

    #[test]
    #[should_panic(expected = "at least one agent")]
    fn a_server_with_no_agents_is_refused() {
        AgentRegistry::new(HashMap::new());
    }
}
