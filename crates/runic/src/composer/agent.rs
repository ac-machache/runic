use std::sync::Arc;

use runic_agent::Llm;

use crate::ability::Ability;

#[derive(Clone)]
pub struct Agent {
    pub(crate) llm: Llm,
    pub(crate) abilities: Vec<Arc<dyn Ability>>,
    pub(crate) output_schema: Option<serde_json::Value>,
}

impl Agent {
    pub fn new(llm: Llm) -> Self {
        Self {
            llm,
            abilities: Vec::new(),
            output_schema: None,
        }
    }

    pub fn with(mut self, ability: impl Ability + 'static) -> Self {
        self.abilities.push(Arc::new(ability));
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
    ) -> Result<runic_agent::Session, super::ComposeError> {
        super::Composer::new(self.clone(), super::Runtime::new())
            .build(tenant, session)
            .await
    }
}
