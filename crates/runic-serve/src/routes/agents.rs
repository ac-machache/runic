//! Runner discovery — the registry of named agents this server hosts.

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use utoipa::ToSchema;

use crate::app::AppState;
use crate::error::ServeError;

const INTROSPECTION_TENANT: &str = "__introspect__";
const INTROSPECTION_SESSION: &str = "__introspect__";

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

#[derive(Debug, Serialize, ToSchema)]
pub struct ToolOverview {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SkillOverview {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SubagentOverview {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AbilityOverview {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub deferred: bool,
    pub activated: bool,
    pub tools: Vec<ToolOverview>,
    pub skills: Vec<SkillOverview>,
    pub subagents: Vec<SubagentOverview>,
    pub hooks: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AgentOverview {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    pub abilities: Vec<AbilityOverview>,
}

fn ability_overview(view: runic::composer::AbilityView) -> AbilityOverview {
    AbilityOverview {
        id: view.id,
        name: view.name,
        description: view.description,
        deferred: view.deferred,
        activated: view.activated,
        tools: view
            .tools
            .into_iter()
            .map(|spec| ToolOverview {
                name: spec.name,
                description: spec.description,
                parameters: spec.parameters,
            })
            .collect(),
        skills: view
            .skills
            .into_iter()
            .map(|skill| SkillOverview {
                id: skill.id,
                description: skill.description,
            })
            .collect(),
        subagents: view
            .subagents
            .into_iter()
            .map(|subagent| SubagentOverview {
                name: subagent.name,
                description: subagent.description,
            })
            .collect(),
        hooks: view.hooks,
    }
}

/// `GET /agents/{name}` — structural view of a named agent: its abilities and
/// the tools/skills/subagents/hooks each carries. Resolves a throwaway instance
/// to read them; nothing is persisted.
#[utoipa::path(
    get,
    path = "/agents/{name}",
    tag = "agents",
    params(("name" = String, Path, description = "Registered agent name")),
    responses(
        (status = 200, description = "Structural view of the agent", body = AgentOverview),
        (status = 404, description = "No agent registered under this name", body = crate::error::ErrorBody)
    )
)]
pub async fn agent_overview(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<AgentOverview>, ServeError> {
    let hosted = state.agents.get(&name)?.clone();
    let views = runic::Composer::new(hosted.agent.clone())
        .describe(INTROSPECTION_TENANT, INTROSPECTION_SESSION)
        .await
        .map_err(|error| ServeError::Runner(error.to_string()))?;
    let runner = hosted
        .agent
        .build(INTROSPECTION_TENANT, INTROSPECTION_SESSION)
        .await
        .map_err(|error| ServeError::Runner(error.to_string()))?;

    Ok(Json(AgentOverview {
        name,
        model: Some(runner.model().to_string()),
        max_turns: Some(runner.max_turns()),
        abilities: views.into_iter().map(ability_overview).collect(),
    }))
}
