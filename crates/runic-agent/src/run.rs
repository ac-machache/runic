//! The OUTER loop. Owns the turn counter, usage accumulation, the
//! `RunStart`/`RunEnd` bookends, and stop handling. Per-turn work is delegated
//! to [`crate::turn::run_one_turn`]; tool dispatch to [`Runner::dispatch_tools`].
//!
//! [`Runner::run_message_with`] installs the per-run [`RunContext`] (config map,
//! provider override, cancellation, steering), runs the loop, then restores.

use chrono::Utc;

use runic_state::{RunOutcome, new_run_id};
use runic_types::{ContentBlock, Message, StopReason, TokenUsage};
use tokio::sync::mpsc;

use tracing::Instrument;

use crate::turn::Point;
use crate::{AgentError, CancelToken, RunContext, Runner};

impl Runner {
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
        ctx: RunContext,
    ) -> Result<RunOutcome, AgentError> {
        self.drive(Some(user_msg), ctx).await
    }

    pub async fn resume(&mut self, ctx: RunContext) -> Result<RunOutcome, AgentError> {
        self.drive(None, ctx).await
    }

    async fn drive(
        &mut self,
        user_msg: Option<Message>,
        mut ctx: RunContext,
    ) -> Result<RunOutcome, AgentError> {
        self.state.config = std::mem::take(&mut ctx.config);
        self.clear_transient_tool_outputs();
        self.pending_deferral = None;
        // Provider override is restored after the run.
        let saved_provider = ctx
            .provider
            .take()
            .map(|p| std::mem::replace(&mut self.provider, p));
        if let Some(emitter) = ctx.events.take() {
            self.state.set_emitter(Some(emitter));
        }
        self.sub_session = ctx.sub_session.take();
        let cancel = ctx.cancel.take();
        let mut steering = ctx.steering.take();
        let agent_label = ctx.agent.take();
        let actor = ctx.actor.take();
        let run_id = ctx.run_id.take().unwrap_or_else(new_run_id);

        let span = tracing::info_span!(
            "run",
            run_id = %run_id,
            tenant = %self.state.user_id,
            thread = %self.state.session_id,
            mode = ctx.mode.unwrap_or("direct"),
            total_turns = tracing::field::Empty,
            input_tokens = tracing::field::Empty,
            output_tokens = tracing::field::Empty,
            stop_reason = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        let result = self
            .run_loop(
                user_msg,
                run_id,
                agent_label,
                actor,
                cancel.as_ref(),
                steering.as_mut(),
            )
            .instrument(span.clone())
            .await;
        match &result {
            Ok(outcome) => {
                span.record("total_turns", outcome.total_turns);
                span.record("input_tokens", outcome.usage.input_tokens);
                span.record("output_tokens", outcome.usage.output_tokens);
                span.record("stop_reason", outcome.stop_reason.as_deref().unwrap_or("-"));
            }
            Err(e) => {
                span.record("stop_reason", tracing::field::display(e));
                span.record("otel.status_code", "ERROR");
            }
        }

        self.state.set_emitter(None);
        self.sub_session = None;
        self.clear_transient_tool_outputs();
        if let Some(p) = saved_provider {
            self.provider = p;
        }
        result
    }

    pub(crate) fn clear_transient_tool_outputs(&mut self) {
        self.transient_tool_outputs.clear();
    }

    /// The turn loop proper.
    async fn run_loop(
        &mut self,
        user_msg: Option<Message>,
        run_id: String,
        agent_label: Option<String>,
        actor: Option<String>,
        cancel: Option<&CancelToken>,
        steering: Option<&mut mpsc::UnboundedReceiver<String>>,
    ) -> Result<RunOutcome, AgentError> {
        self.guard.reset();

        let fire_before_agent = user_msg.is_some();
        if let Some(user_msg) = user_msg {
            let now = Utc::now();
            self.emit(crate::AgentEvent::RunStarted {
                run_id: run_id.clone(),
                agent: agent_label,
                audit: Some(runic_state::AuditStamp {
                    model: Some(self.config.model.clone()),
                    actor: actor.map(|value| value.chars().take(128).collect()),
                }),
                at: now,
            });
            self.emit(crate::AgentEvent::Message {
                run_id: run_id.clone(),
                msg: user_msg,
                at: now,
            });
        }
        tracing::info!(%run_id, user_id = %self.state.user_id, session_id = %self.state.session_id, "run started");

        let mut totals = LoopTotals::default();
        let result = self
            .run_loop_inner(fire_before_agent, &run_id, cancel, steering, &mut totals)
            .await;
        self.finalize_run(run_id, result, totals)
    }

    /// The ONLY place a terminal `RunEnd` is emitted — every exit path of
    /// `run_loop_inner` (success, cancellation, any hook/provider/dispatch
    /// failure) funnels through here exactly once. Suspension is not terminal.
    fn finalize_run(
        &mut self,
        run_id: String,
        result: Result<String, AgentError>,
        totals: LoopTotals,
    ) -> Result<RunOutcome, AgentError> {
        match result {
            Ok(stop_reason) if stop_reason == "suspended" => {
                let deferral = self
                    .pending_deferral
                    .take()
                    .expect("a suspended run always carries its deferral");
                self.emit(crate::AgentEvent::ToolDeferred {
                    run_id: run_id.clone(),
                    call_id: deferral.call_id,
                    tool: deferral.tool,
                    payload: deferral.payload,
                    at: Utc::now(),
                });
                tracing::info!(%run_id, turns = totals.turns, "run suspended");
                Ok(RunOutcome {
                    total_turns: totals.turns,
                    stop_reason: Some("suspended".to_string()),
                    usage: totals.usage,
                    structured: None,
                })
            }
            Ok(stop_reason) => {
                let outcome = RunOutcome {
                    total_turns: totals.turns,
                    stop_reason: Some(stop_reason),
                    usage: totals.usage,
                    structured: totals.structured,
                };
                tracing::info!(
                    %run_id,
                    turns = totals.turns,
                    stop_reason = outcome.stop_reason.as_deref().unwrap_or("-"),
                    input_tokens = outcome.usage.input_tokens,
                    output_tokens = outcome.usage.output_tokens,
                    "run completed"
                );
                let status = if outcome.stop_reason.as_deref() == Some("cancelled") {
                    runic_state::RunEndStatus::Cancelled
                } else {
                    runic_state::RunEndStatus::Completed
                };
                self.emit(crate::AgentEvent::RunEnd {
                    run_id,
                    status,
                    outcome: outcome.clone(),
                    at: Utc::now(),
                });
                Ok(outcome)
            }
            Err(e) => {
                tracing::error!(%run_id, turns = totals.turns, error = %e, "run failed");
                self.emit(crate::AgentEvent::RunEnd {
                    run_id,
                    status: runic_state::RunEndStatus::Failed(e.to_string()),
                    outcome: RunOutcome {
                        total_turns: totals.turns,
                        stop_reason: Some(format!("error: {e}")),
                        usage: totals.usage,
                        structured: None,
                    },
                    at: Utc::now(),
                });
                Err(e)
            }
        }
    }

    async fn run_loop_inner(
        &mut self,
        fire_before_agent: bool,
        run_id: &str,
        cancel: Option<&CancelToken>,
        mut steering: Option<&mut mpsc::UnboundedReceiver<String>>,
        totals: &mut LoopTotals,
    ) -> Result<String, AgentError> {
        if fire_before_agent {
            self.fire_write(run_id, Point::BeforeAgent).await?;
            self.fire_read(run_id, Point::BeforeAgent).await?;
        }

        // The loop yields the final stop-reason string, or an error.
        let result: Result<String, AgentError> = loop {
            // Cancellation — graceful, at the turn boundary.
            if cancel.is_some_and(|c| c.is_cancelled()) {
                tracing::info!(%run_id, "run cancelled");
                break Ok("cancelled".to_string());
            }

            while let Ok(ev) = self.fold_rx.try_recv() {
                self.state.fold(&ev);
            }

            // Steering — inject any pending nudges as user messages.
            if let Some(rx) = steering.as_deref_mut() {
                let mut nudges = Vec::new();
                while let Ok(text) = rx.try_recv() {
                    nudges.push(text);
                }
                for text in nudges {
                    self.emit(crate::AgentEvent::Message {
                        run_id: run_id.to_string(),
                        msg: Message::user(text),
                        at: Utc::now(),
                    });
                }
            }

            // Turn backstop.
            if totals.turns >= self.config.max_turns {
                tracing::warn!(
                    %run_id,
                    max_turns = self.config.max_turns,
                    graceful = self.config.graceful_max_turns,
                    "max turns reached"
                );
                if self.config.graceful_max_turns {
                    match self.finish_summary(run_id, totals.turns + 1).await {
                        Ok(usage) => {
                            totals.turns += 1;
                            add_usage(&mut totals.usage, &usage);
                            break Ok("max_turns".to_string());
                        }
                        Err(e) => break Err(e),
                    }
                }
                break Err(AgentError::MaxTurnsExceeded(self.config.max_turns));
            }

            tracing::debug!(%run_id, turn = totals.turns + 1, "turn started");
            let turn = match self
                .run_one_turn(run_id, totals.turns + 1)
                .instrument(tracing::info_span!("turn", n = totals.turns + 1))
                .await
            {
                Ok(t) => t,
                Err(e) => break Err(e),
            };

            totals.turns += 1;
            add_usage(&mut totals.usage, &turn.usage);
            tracing::debug!(
                %run_id,
                turn = totals.turns,
                stop_reason = stop_reason_str(turn.stop_reason),
                "turn completed"
            );

            if self.config.output_schema.is_some()
                && let Some(call) = turn
                    .tool_calls
                    .iter()
                    .find(|c| c.name == crate::FINAL_ANSWER_TOOL)
            {
                totals.structured = Some(call.input.clone());
                self.push_tool_results(
                    Message::user_with_blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: call.id.clone(),
                        tool_name: crate::FINAL_ANSWER_TOOL.to_string(),
                        content: "Recorded.".into(),
                        is_error: false,
                        provenance: Vec::new(),
                    }]),
                    run_id,
                );
                break Ok("final_answer".to_string());
            }

            if turn.tool_calls.is_empty() {
                break Ok(stop_reason_str(turn.stop_reason).to_string());
            }

            let dispatch_span = tracing::info_span!(
                "dispatch",
                batch = turn.tool_calls.len(),
                errors = tracing::field::Empty,
            );
            if let Err(e) = self
                .dispatch_tools(turn.tool_calls, run_id, totals.turns)
                .instrument(dispatch_span)
                .await
            {
                break Err(e);
            }
            if self.pending_deferral.is_some() {
                break Ok("suspended".to_string());
            }
        };

        let stop_reason = result?;
        if stop_reason != "suspended" {
            self.fire_write(run_id, Point::AfterAgent).await?;
            self.fire_read(run_id, Point::AfterAgent).await?;
        }
        Ok(stop_reason)
    }

    /// One final tools-free model call to extract a best-effort answer when the
    /// turn backstop trips (graceful mode). It is a real turn: it costs money,
    /// so it gets its own `TurnEnd`.
    async fn finish_summary(
        &mut self,
        run_id: &str,
        turn_number: u32,
    ) -> Result<TokenUsage, AgentError> {
        self.emit(crate::AgentEvent::Message {
            run_id: run_id.to_string(),
            msg: Message::user(
                "You've reached the step limit. Give your best final answer now \
                 using what you already have — do not call any tools.",
            ),
            at: Utc::now(),
        });
        let mut request = self.prepare_request();
        request.tools.clear(); // force a text answer
        let started = std::time::Instant::now();
        let (response, model) = self.call_model(request).await?;
        let model_ms = started.elapsed().as_millis() as u64;
        let (assistant, turn) = Self::interpret_response(response, model, model_ms);
        self.push_assistant(assistant, run_id);
        self.emit(crate::AgentEvent::TurnEnd {
            run_id: run_id.to_string(),
            turn: turn_number,
            model: turn.model.clone(),
            usage: turn.usage,
            model_ms: turn.model_ms,
            stop_reason: String::new(),
            at: Utc::now(),
        });
        Ok(turn.usage)
    }
}

#[derive(Default)]
struct LoopTotals {
    turns: u32,
    usage: TokenUsage,
    structured: Option<serde_json::Value>,
}

fn add_usage(total: &mut TokenUsage, delta: &TokenUsage) {
    total.add(delta);
}

pub(crate) fn stop_reason_str(s: StopReason) -> &'static str {
    match s {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
    }
}
