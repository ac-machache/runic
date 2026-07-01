//! One model turn — the thin orchestrator (ZeroClaw's step-decomposition).
//!
//! [`Agent::run_one_turn`] reads top-to-bottom as a flat list of named steps;
//! each step is a focused function in its own sibling module.

mod dispatch;
mod history;
mod hooks;
mod provider_call;
mod request;
mod response;

pub(crate) use hooks::Point;

use crate::{Agent, AgentError, TurnRecord};

impl Agent {
    /// Drive a single model turn: hooks → request → model → record → hooks.
    /// Tool dispatch (when the turn requests tools) is driven by the outer
    /// loop via [`Agent::dispatch_tools`].
    pub(crate) async fn run_one_turn(&mut self, run_id: &str) -> Result<TurnRecord, AgentError> {
        self.fire_write(run_id, Point::BeforeModel).await?; // hooks (sequential)
        self.fire_read(run_id, Point::BeforeModel).await?; //        (parallel)

        let request = self.prepare_request(); // request.rs
        tracing::debug!(
            run_id,
            messages = request.messages.len(),
            tools = request.tools.len(),
            "model request prepared"
        );
        let response = self.call_model(request).await?; // provider_call.rs (retry)
        tracing::debug!(
            run_id,
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            "model response received"
        );

        let (assistant, turn) = Self::interpret_response(response); // response.rs
        self.push_assistant(assistant, run_id); // history.rs — state now has the reply

        self.fire_write(run_id, Point::AfterModel).await?; // hooks see the reply
        self.fire_read(run_id, Point::AfterModel).await?;

        Ok(turn)
    }
}
