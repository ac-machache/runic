use std::sync::Arc;

use runic_agent::Llm;
use runic_substrate::ArtifactStore;

use crate::ability::Ability;

#[derive(Clone)]
pub struct Agent {
    pub(crate) llm: Llm,
    pub(crate) abilities: Vec<Arc<dyn Ability>>,
    pub(crate) output_schema: Option<serde_json::Value>,
    pub(crate) artifact_store: Option<Arc<dyn ArtifactStore>>,
}

impl Agent {
    pub fn new(llm: Llm) -> Self {
        Self {
            llm,
            abilities: Vec::new(),
            output_schema: None,
            artifact_store: None,
        }
    }

    pub fn with(mut self, ability: impl Ability + 'static) -> Self {
        self.abilities.push(Arc::new(ability));
        self
    }

    pub fn artifacts(mut self, store: Arc<dyn ArtifactStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    pub fn output<T: schemars::JsonSchema>(self) -> Self {
        let schema = runic_agent::schema_of::<T>();
        self.output_schema(schema)
    }

    pub fn output_schema(mut self, schema: serde_json::Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    pub async fn build(
        &self,
        tenant: &str,
        session: &str,
    ) -> Result<runic_agent::Runner, super::ComposeError> {
        let mut runtime = super::Runtime::new();
        if let Some(store) = &self.artifact_store {
            runtime = runtime.artifacts(store.clone());
        }
        super::Composer::new(self.clone(), runtime)
            .build(tenant, session)
            .await
    }

    pub async fn run(&self, message: impl Into<String>) -> anyhow::Result<AgentOutput> {
        let mut runner = self.build("local", "local").await?;
        let outcome = runner
            .run(message.into())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(AgentOutput::from_run(&runner, outcome))
    }

    pub async fn stream(
        &self,
        message: impl Into<String>,
        events: Arc<dyn runic_state::Emitter>,
    ) -> anyhow::Result<AgentOutput> {
        let mut runner = self.build("local", "local").await?;
        let ctx = runic_agent::RunContext::new().with_events(events);
        let outcome = runner
            .run_with(message.into(), ctx)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(AgentOutput::from_run(&runner, outcome))
    }

    pub fn session(
        &self,
        store: Arc<dyn runic_substrate::SessionStore>,
        tenant: impl Into<String>,
        session: impl Into<String>,
    ) -> super::Session {
        super::Session::new(self.clone(), store, tenant.into(), session.into())
    }
}

pub struct AgentOutput {
    pub text: String,
    pub outcome: runic_agent::RunOutcome,
}

impl AgentOutput {
    pub(crate) fn from_run(runner: &runic_agent::Runner, outcome: runic_agent::RunOutcome) -> Self {
        Self {
            text: runner.state().last_assistant_text().unwrap_or_default(),
            outcome,
        }
    }

    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> anyhow::Result<T> {
        let Some(value) = &self.outcome.structured else {
            anyhow::bail!(
                "no structured output (stop_reason: {:?})",
                self.outcome.stop_reason
            );
        };
        Ok(serde_json::from_value(value.clone())?)
    }
}
