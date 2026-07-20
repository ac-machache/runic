use runic_agent::RunOutcome;
use serde::de::DeserializeOwned;

pub(crate) use runic_agent::schema_of;

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
