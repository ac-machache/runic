use runic_agent::RunOutcome;
use serde::de::DeserializeOwned;

pub(crate) fn schema_of<T: schemars::JsonSchema>() -> serde_json::Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(T)).unwrap_or_default();
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
    }
    schema
}

pub trait StructuredOutput {
    fn output_as<T: DeserializeOwned>(&self) -> anyhow::Result<T>;
}

impl StructuredOutput for RunOutcome {
    fn output_as<T: DeserializeOwned>(&self) -> anyhow::Result<T> {
        let Some(value) = &self.structured else {
            anyhow::bail!(
                "run produced no structured output (stop_reason: {:?})",
                self.stop_reason
            );
        };
        Ok(serde_json::from_value(value.clone())?)
    }
}
