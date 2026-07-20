//! One model turn — the thin orchestrator (ZeroClaw's step-decomposition).
//!
//! [`Session::run_one_turn`] reads top-to-bottom as a flat list of named steps;
//! each step is a focused function in its own sibling module.

mod dispatch;
mod history;
mod hooks;
mod provider_call;
mod request;
mod response;

pub(crate) use hooks::Point;

use crate::{AgentError, Session, TurnRecord};

impl Session {
    /// Drive a single model turn: hooks → request → model → record → hooks.
    /// Tool dispatch (when the turn requests tools) is driven by the outer
    /// loop via [`Session::dispatch_tools`].
    pub(crate) async fn run_one_turn(
        &mut self,
        run_id: &str,
        turn_number: u32,
    ) -> Result<TurnRecord, AgentError> {
        self.refresh_activated_tools();
        self.fire_write(run_id, Point::BeforeModel).await?; // hooks (sequential)
        self.fire_read(run_id, Point::BeforeModel).await?; //        (parallel)

        let request = self.prepare_request(); // request.rs
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

        let (assistant, turn) = Self::interpret_response(response, model, model_ms); // response.rs
        self.push_assistant(assistant, run_id); // history.rs — state now has the reply

        // The turn's durable accounting lands BEFORE the after-model hooks —
        // a hook failure must not erase a model call that already cost money.
        self.state.push_event(runic_state::SessionEvent::TurnEnd {
            run_id: run_id.to_string(),
            turn: turn_number,
            model: turn.model.clone(),
            usage: turn.usage,
            model_ms: turn.model_ms,
            at: chrono::Utc::now(),
        });
        self.emit(crate::AgentEvent::TurnEnd {
            run_id: run_id.to_string(),
            turn: turn_number,
            model: turn.model.clone(),
            usage: turn.usage,
            model_ms: turn.model_ms,
            stop_reason: crate::run::stop_reason_str(turn.stop_reason).to_string(),
            at: chrono::Utc::now(),
        });

        self.fire_write(run_id, Point::AfterModel).await?; // hooks see the reply
        self.fire_read(run_id, Point::AfterModel).await?;

        Ok(turn)
    }
}
