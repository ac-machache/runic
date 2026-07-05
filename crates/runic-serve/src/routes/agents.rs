//! Agent discovery — the registry of named agents this server hosts.

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use crate::app::AppState;

#[derive(Debug, Serialize, ToSchema)]
pub struct AgentInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AgentList {
    pub agents: Vec<AgentInfo>,
}

/// `GET /agents` — every registered agent, sorted by name.
#[utoipa::path(
    get,
    path = "/agents",
    tag = "agents",
    responses((status = 200, description = "The registered agents", body = AgentList))
)]
pub async fn list_agents(State(state): State<AppState>) -> Json<AgentList> {
    let agents = state
        .agents
        .agent_names()
        .into_iter()
        .map(|(name, description)| AgentInfo {
            name: name.to_string(),
            description: description.map(str::to_string),
        })
        .collect();
    Json(AgentList { agents })
}
