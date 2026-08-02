use crate::app::AppState;
use crate::error::ServeError;
use crate::store::Runs;
use chrono::{DateTime, Utc};

pub struct RoutineContext {
    pub schedule_id: String,
    pub tenant: Option<String>,
    pub payload: serde_json::Value,
    pub fired_at: DateTime<Utc>,
    state: AppState,
}

impl RoutineContext {
    pub(crate) fn new(
        state: AppState,
        schedule_id: String,
        tenant: Option<String>,
        payload: Option<serde_json::Value>,
    ) -> Self {
        Self {
            schedule_id,
            tenant,
            payload: payload.unwrap_or(serde_json::Value::Null),
            fired_at: Utc::now(),
            state,
        }
    }
    pub fn runs(&self) -> &Runs {
        self.state.runs()
    }

    pub fn session(&self, tenant: &str, session_id: &str) -> runic::Session {
        self.state.session(tenant, session_id)
    }

    pub fn scratch(&self, tenant: &str, session_id: &str) -> runic::Session {
        self.state.scratch(tenant, session_id)
    }

    pub fn agent(&self, name: &str) -> Result<runic::Agent, ServeError> {
        Ok(self.state.agents.get(name)?.agent.clone())
    }

    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> anyhow::Result<T> {
        Ok(serde_json::from_value(self.payload.clone())?)
    }
}
