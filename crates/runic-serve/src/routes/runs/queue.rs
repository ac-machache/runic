use super::input::RunMessageRequest;
use crate::app::AppState;
use crate::error::ServeError;
use crate::store::RunSpec;

pub(crate) async fn enqueue(
    state: &AppState,
    tenant: &str,
    run_id: &str,
    session_id: Option<&str>,
    req: RunMessageRequest,
) -> Result<String, ServeError> {
    let agent = state.agents.resolve_agent(req.agent.as_deref())?;
    let context = req.context.clone();
    let hook = req.hook.clone();
    if let Some(name) = &hook
        && !state.hooks.knows(name)
    {
        return Err(ServeError::BadRequest(format!(
            "no run hook named {name:?} is registered; known hooks: {:?}",
            state.hooks.names()
        )));
    }
    let message = req.into_message()?;

    let payload = serde_json::to_value(&message)
        .map_err(|error| ServeError::Internal(format!("could not encode the turn: {error}")))?;
    let mut spec = RunSpec::new(tenant, run_id, &agent)
        .input(payload)
        .context(context)
        .hook(hook);
    if let Some(session_id) = session_id {
        spec = spec.session(session_id);
    }

    state
        .runs()
        .enqueue(&spec)
        .await
        .map_err(|error| ServeError::Internal(format!("could not queue the run: {error}")))?;
    Ok(agent)
}
