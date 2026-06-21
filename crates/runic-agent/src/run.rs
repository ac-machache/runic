//! The OUTER loop. Owns the turn counter, usage accumulation, the
//! `RunStart`/`RunEnd` bookends, and stop handling. Per-turn work is delegated
//! to [`crate::turn::run_one_turn`]; tool dispatch to [`Agent::dispatch_tools`].
//!
//! [`Agent::run_message_with`] installs the per-run [`RunContext`] (config map,
//! provider override, cancellation, steering), runs the loop, then restores.

use chrono::Utc;

use runic_state::{RunOutcome, SessionEvent, new_run_id};
use runic_types::{Message, StopReason, TokenUsage};
use tokio::sync::mpsc;

use crate::turn::Point;
use crate::{Agent, AgentError, CancelToken, RunContext};

impl Agent {
    /// Run one user turn to completion (text in, [`RunOutcome`] out).
    pub async fn run(&mut self, input: impl Into<String>) -> Result<RunOutcome, AgentError> {
        self.run_message_with(Message::user(input.into()), RunContext::default())
            .await
    }

    /// Run starting from an already-built user [`Message`].
    pub async fn run_message(&mut self, user_msg: Message) -> Result<RunOutcome, AgentError> {
        self.run_message_with(user_msg, RunContext::default()).await
    }

    /// Run with a per-run [`RunContext`] (config / provider override /
    /// cancellation / steering).
    pub async fn run_with(
        &mut self,
        input: impl Into<String>,
        ctx: RunContext,
    ) -> Result<RunOutcome, AgentError> {
        self.run_message_with(Message::user(input.into()), ctx)
            .await
    }

    /// The full entry point. Installs the per-run context, drives the loop,
    /// and restores the build-time provider afterwards (on success *and*
    /// error).
    pub async fn run_message_with(
        &mut self,
        user_msg: Message,
        mut ctx: RunContext,
    ) -> Result<RunOutcome, AgentError> {
        // Per-run config overwrites the map every run, so it can't leak.
        self.state.config = std::mem::take(&mut ctx.config);
        // Provider override is restored after the run.
        let saved_provider = ctx
            .provider
            .take()
            .map(|p| std::mem::replace(&mut self.provider, p));
        self.events = ctx.events.take();
        self.human = ctx.human.take();
        let cancel = ctx.cancel.take();
        let mut steering = ctx.steering.take();

        let result = self
            .run_loop(user_msg, cancel.as_ref(), steering.as_mut())
            .await;

        self.events = None; // drop the sink (closes the receiver)
        self.human = None; // drop the per-run human channel
        if let Some(p) = saved_provider {
            self.provider = p;
        }
        result
    }

    /// The turn loop proper.
    async fn run_loop(
        &mut self,
        user_msg: Message,
        cancel: Option<&CancelToken>,
        mut steering: Option<&mut mpsc::UnboundedReceiver<String>>,
    ) -> Result<RunOutcome, AgentError> {
        let run_id = new_run_id();
        self.guard.reset();

        let now = Utc::now();
        self.state.push_event(SessionEvent::RunStart {
            run_id: run_id.clone(),
            at: now,
        });
        self.state.push_event(SessionEvent::Message {
            run_id: run_id.clone(),
            msg: user_msg,
            at: now,
        });
        self.emit(crate::AgentEvent::RunStarted {
            run_id: run_id.clone(),
        });

        self.fire_write(Point::BeforeAgent).await?;
        self.fire_read(Point::BeforeAgent).await?;

        let mut total_turns: u32 = 0;
        let mut total_usage = TokenUsage::default();

        // The loop yields the final stop-reason string, or an error.
        let result: Result<String, AgentError> = loop {
            // Cancellation — graceful, at the turn boundary.
            if cancel.is_some_and(|c| c.is_cancelled()) {
                break Ok("cancelled".to_string());
            }

            // Steering — inject any pending nudges as user messages.
            if let Some(rx) = steering.as_deref_mut() {
                while let Ok(text) = rx.try_recv() {
                    self.state.push_event(SessionEvent::Message {
                        run_id: run_id.clone(),
                        msg: Message::user(text),
                        at: Utc::now(),
                    });
                }
            }

            // Turn backstop.
            if total_turns >= self.config.max_turns {
                if self.config.graceful_max_turns {
                    match self.finish_summary(&run_id).await {
                        Ok(usage) => {
                            add_usage(&mut total_usage, &usage);
                            break Ok("max_turns".to_string());
                        }
                        Err(e) => break Err(e),
                    }
                }
                break Err(AgentError::MaxTurnsExceeded(self.config.max_turns));
            }

            let turn = match self.run_one_turn(&run_id).await {
                Ok(t) => t,
                Err(e) => break Err(e),
            };

            total_turns += 1;
            add_usage(&mut total_usage, &turn.usage);

            self.state.push_event(SessionEvent::TurnBoundary {
                run_id: run_id.clone(),
                at: Utc::now(),
            });
            self.emit(crate::AgentEvent::TurnCompleted {
                turn: total_turns,
                stop_reason: stop_reason_str(turn.stop_reason).to_string(),
            });

            if turn.tool_calls.is_empty() {
                break Ok(stop_reason_str(turn.stop_reason).to_string());
            }

            if let Err(e) = self.dispatch_tools(turn.tool_calls, &run_id).await {
                break Err(e);
            }
        };

        match result {
            Ok(stop_reason) => {
                self.fire_write(Point::AfterAgent).await?;
                self.fire_read(Point::AfterAgent).await?;
                let outcome = RunOutcome {
                    total_turns,
                    stop_reason: Some(stop_reason),
                    usage: total_usage,
                };
                self.state.push_event(SessionEvent::RunEnd {
                    run_id,
                    outcome: outcome.clone(),
                    at: Utc::now(),
                });
                self.emit(crate::AgentEvent::RunCompleted(outcome.clone()));
                Ok(outcome)
            }
            Err(e) => {
                self.state.push_event(SessionEvent::RunEnd {
                    run_id,
                    outcome: RunOutcome {
                        total_turns,
                        stop_reason: Some(format!("error: {e}")),
                        usage: total_usage,
                    },
                    at: Utc::now(),
                });
                Err(e)
            }
        }
    }

    /// One final tools-free model call to extract a best-effort answer when the
    /// turn backstop trips (graceful mode).
    async fn finish_summary(&mut self, run_id: &str) -> Result<TokenUsage, AgentError> {
        self.state.push_event(SessionEvent::Message {
            run_id: run_id.to_string(),
            msg: Message::user(
                "You've reached the step limit. Give your best final answer now \
                 using what you already have — do not call any tools.",
            ),
            at: Utc::now(),
        });
        let mut request = self.prepare_request();
        request.tools.clear(); // force a text answer
        let response = self.call_model(request).await?;
        let (assistant, turn) = Self::interpret_response(response);
        self.push_assistant(assistant, run_id);
        Ok(turn.usage)
    }
}

fn add_usage(total: &mut TokenUsage, delta: &TokenUsage) {
    total.input_tokens += delta.input_tokens;
    total.output_tokens += delta.output_tokens;
}

fn stop_reason_str(s: StopReason) -> &'static str {
    match s {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
    }
}
