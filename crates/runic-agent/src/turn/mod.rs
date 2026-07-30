//! One model turn — the thin orchestrator (ZeroClaw's step-decomposition).
//!
//! [`Runner::run_one_turn`] reads top-to-bottom as a flat list of named steps;
//! each step is a focused function in its own sibling module.

mod dispatch;
mod history;
mod hooks;
mod provider_call;
mod request;
mod response;

pub(crate) use hooks::Point;

use crate::{AgentError, Runner, TurnRecord};

impl Runner {
    /// Drive a single model turn: hooks → request → model → record → hooks.
    /// Tool dispatch (when the turn requests tools) is driven by the outer
    /// loop via [`Runner::dispatch_tools`].
    pub(crate) async fn run_one_turn(
        &mut self,
        run_id: &str,
        turn_number: u32,
    ) -> Result<TurnRecord, AgentError> {
        self.refresh_activated_tools();
        let mut request = self.prepare_request(); // request.rs
        self.fire_write_before_model(run_id, &mut request).await?; // (sequential)
        self.fire_read_before_model(run_id, &request).await?; //      (observe)
        tracing::debug!(
            run_id,
            messages = request.messages.len(),
            tools = request.tools.len(),
            "model request prepared"
        );
        let started = std::time::Instant::now();
        let (response, model) = self.call_model(request).await?; // provider_call.rs (retry)
        let model_ms = started.elapsed().as_millis() as u64;
        tracing::debug!(
            run_id,
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            "model response received"
        );

        let mut response = response;
        let mut aborted = self
            .fire_write_after_model(run_id, &mut response)
            .await
            .err();
        if aborted.is_none() {
            response.tool_calls = Self::tool_calls_of(&response.content);
            if response.tool_calls.is_empty()
                && response.stop_reason == runic_types::StopReason::ToolUse
            {
                response.stop_reason = runic_types::StopReason::EndTurn;
            }
            aborted = self.fire_read_after_model(run_id, &response).await.err();
        }

        let (assistant, turn) = Self::interpret_response(response, model, model_ms); // response.rs
        self.push_assistant(assistant, run_id); // history.rs — state now has the reply

        self.emit(crate::AgentEvent::TurnEnd {
            run_id: run_id.to_string(),
            turn: turn_number,
            model: turn.model.clone(),
            usage: turn.usage,
            model_ms: turn.model_ms,
            stop_reason: crate::run::stop_reason_str(turn.stop_reason).to_string(),
            at: chrono::Utc::now(),
        });

        if let Some(error) = aborted {
            return Err(error);
        }

        Ok(turn)
    }
}
