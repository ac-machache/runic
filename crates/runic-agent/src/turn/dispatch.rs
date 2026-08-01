//! Step: tool dispatch — runic's 3-phase model.
//!
//! 1. **Plan** (sequential): per call, run the loop guard + `before_tool`
//!    write hooks (which may rewrite the call, substitute a result, cancel, or
//!    stop) + `before_tool` read hooks. Produces a [`CallPlan`] per call.
//! 2. **Execute**: substituted results are filled directly; `parallelizable`
//!    tools run concurrently via `join_all`; the rest run serially. Every
//!    dispatch is timeout-wrapped.
//! 3. **Collect**: per call, `after_tool` hooks fire — and may rewrite the
//!    result — before it is persisted, emitted, and assembled into a single
//!    user-role message appended to the log.

use std::sync::Arc;
use std::time::Duration;

use runic_hook::HookOutcome;
use runic_state::HookLifecycle;
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{
    ContentBlock, Message, ProvenanceSource, ToolCall, ToolResultPayload, sanitize_provenance,
};
use tracing::Instrument;

use runic_state::ToolStatus;

use crate::loop_guard::Verdict;
use crate::turn::hooks::outcome_kind;
use runic_state::Deferral;

use crate::{AgentError, Runner};

/// What the loop decided to do with one requested tool call.
enum CallPlan {
    /// Skip execution; this result was supplied by a hook or the guard.
    Substituted {
        call: ToolCall,
        result: ToolResult,
        status: ToolStatus,
    },
    /// Run the tool (possibly concurrently). `warning` is a loop-guard nudge
    /// to append to the result.
    Dispatch {
        call: ToolCall,
        parallelizable: bool,
        warning: Option<String>,
    },
}

struct Dispatched {
    result: ToolResult,
    status: ToolStatus,
    duration_ms: u64,
}

impl Dispatched {
    fn undispatched(result: ToolResult, status: ToolStatus) -> Self {
        Self {
            result,
            status,
            duration_ms: 0,
        }
    }
}

impl CallPlan {
    fn call(&self) -> &ToolCall {
        match self {
            CallPlan::Substituted { call, .. } | CallPlan::Dispatch { call, .. } => call,
        }
    }
}

impl Runner {
    /// Drive every tool call the model requested this turn, appending one
    /// combined tool-result message at the end.
    pub(crate) async fn dispatch_tools(
        &mut self,
        calls: Vec<ToolCall>,
        run_id: &str,
        turn: u32,
    ) -> Result<(), AgentError> {
        tracing::debug!(run_id, batch_size = calls.len(), "tool batch started");
        // ── Phase 1: plan ──────────────────────────────────────────────────
        let mut plans: Vec<CallPlan> = Vec::with_capacity(calls.len());
        for mut call in calls {
            let guard_warning = match self.guard.check(&call) {
                Verdict::Allow => None,
                Verdict::Warn(msg) => Some(msg),
                Verdict::Block(msg) => {
                    tracing::warn!(run_id, tool = %call.name, reason = %msg, "loop guard blocked tool call");
                    plans.push(CallPlan::Substituted {
                        result: ToolResult::error(msg),
                        call,
                        status: ToolStatus::GuardBlocked,
                    });
                    continue;
                }
                Verdict::CircuitBreak(msg) => {
                    tracing::warn!(run_id, tool = %call.name, reason = %msg, "loop guard circuit break");
                    return Err(AgentError::CircuitBreak(msg));
                }
            };

            let mut substituted: Option<(ToolResult, ToolStatus)> = None;
            for scoped in self.write_hooks.clone() {
                let h = &scoped.hook;
                if !h.points().contains(&HookLifecycle::BeforeTool) {
                    continue;
                }
                if !scoped.fires_for(&call) {
                    continue;
                }
                let outcome = h.before_tool(&mut self.state, &mut call).await;
                tracing::debug!(
                    hook_name = h.name(),
                    hook_kind = "write",
                    point = "before_tool",
                    priority = h.priority(),
                    outcome = outcome_kind(&outcome),
                    "hook fired"
                );
                self.record_write_hook(run_id, h.name(), HookLifecycle::BeforeTool, &outcome);
                match outcome {
                    HookOutcome::Noop | HookOutcome::Continue => {}
                    HookOutcome::SubstituteToolResult(r) => {
                        tracing::warn!(run_id, tool = %call.name, hook = h.name(), "hook substituted tool result");
                        substituted = Some((r, ToolStatus::Substituted));
                        break;
                    }
                    HookOutcome::Cancel(reason) => {
                        tracing::warn!(run_id, tool = %call.name, hook = h.name(), reason = %reason, "hook cancelled tool call");
                        substituted = Some((ToolResult::error(reason), ToolStatus::Cancelled));
                        break;
                    }
                    HookOutcome::Stop => {
                        tracing::warn!(run_id, tool = %call.name, hook = h.name(), "hook stopped run");
                        return Err(AgentError::HookStop);
                    }
                }
            }

            self.fire_read_before_tool(run_id, &call).await?;

            let plan = match substituted {
                Some((result, status)) => CallPlan::Substituted {
                    call,
                    result,
                    status,
                },
                None => {
                    let parallelizable = self
                        .resolve_tool(&call.name)
                        .map(|t| t.parallelizable())
                        .unwrap_or(false);
                    CallPlan::Dispatch {
                        call,
                        parallelizable,
                        warning: guard_warning,
                    }
                }
            };
            plans.push(plan);
        }

        // ── Phase 2: execute ───────────────────────────────────────────────
        // Announce every call that will actually run (substituted ones never
        // dispatch, so they don't get a Started event). The durable ToolStarted
        // lands here, BEFORE execution — a crash mid-tool leaves evidence.
        for plan in &plans {
            if let CallPlan::Dispatch { call, .. } = plan {
                self.emit(crate::AgentEvent::ToolStarted {
                    run_id: run_id.to_string(),
                    turn,
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                    input: call.input.clone(),
                    at: chrono::Utc::now(),
                });
            }
        }

        let mut results: Vec<Option<Dispatched>> = (0..plans.len()).map(|_| None).collect();

        // Pre-supplied (hook/guard) results.
        for (i, plan) in plans.iter().enumerate() {
            if let CallPlan::Substituted { result, status, .. } = plan {
                results[i] = Some(Dispatched::undispatched(result.clone(), *status));
            }
        }

        // Parallelizable batch — concurrent.
        let timeout = self.config.tool_timeout;
        let parallel_idx: Vec<usize> = plans
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                matches!(
                    p,
                    CallPlan::Dispatch {
                        parallelizable: true,
                        ..
                    }
                )
                .then_some(i)
            })
            .collect();
        if !parallel_idx.is_empty() {
            let futs: Vec<_> = parallel_idx
                .iter()
                .map(|&i| {
                    let call = plans[i].call().clone();
                    let tool = self.resolve_tool(&call.name);
                    let mut ctx = self.tool_context(run_id);
                    ctx.insert(runic_tool::CallId(call.id.clone()));
                    ctx.insert(runic_tool::CurrentTurn(turn));
                    async move { (i, dispatch_one(tool, call, ctx, timeout, true).await) }
                })
                .collect();
            for (i, r) in futures::future::join_all(futs).await {
                results[i] = Some(r);
            }
        }

        // Remaining (non-parallelizable) dispatches — serial.
        for i in 0..plans.len() {
            if let CallPlan::Dispatch {
                parallelizable: false,
                call,
                ..
            } = &plans[i]
            {
                let call = call.clone();
                let tool = self.resolve_tool(&call.name);
                let mut ctx = self.tool_context(run_id);
                ctx.insert(runic_tool::CallId(call.id.clone()));
                ctx.insert(runic_tool::CurrentTurn(turn));
                results[i] = Some(dispatch_one(tool, call, ctx, timeout, false).await);
            }
        }

        // ── Phase 3: collect + after_tool hooks ────────────────────────────
        let mut blocks: Vec<ContentBlock> = Vec::with_capacity(plans.len());
        let mut aborted: Option<AgentError> = None;
        for (i, plan) in plans.iter().enumerate() {
            let call = plan.call();
            let dispatched = results[i].take().expect("every plan produced a result");
            let Dispatched {
                result,
                status,
                duration_ms,
            } = dispatched;

            let mut result = match result {
                ToolResult::Deferred { payload } => {
                    self.defer(call, payload);
                    continue;
                }
                other => other,
            };

            // For actually-dispatched calls: feed the outcome to the guard
            // (so identical call+result streaks escalate) and append any nudge.
            if let CallPlan::Dispatch { warning, .. } = plan {
                let outcome_text = result.text();
                let mut notes: Vec<String> = Vec::new();
                if let Some(outcome_warning) = self.guard.record_outcome(call, &outcome_text) {
                    notes.push(format!("[loop guard] {outcome_warning}"));
                }
                if let Some(w) = warning {
                    notes.push(format!("[loop guard] {w}"));
                }
                result.push_notes(&notes);
            }

            // The tool already ran, so there's no call to make in-band: both
            // `Stop` and `Cancel` halt the run (matching every non-`before_tool`
            // seam). Only `before_tool`'s `Cancel` is the skip-and-continue case.
            // The halt is deferred until every result is recorded, so history
            // never keeps a `tool_use` without its `tool_result`.
            if aborted.is_none() {
                for scoped in self.write_hooks.clone() {
                    let h = &scoped.hook;
                    if !h.points().contains(&HookLifecycle::AfterTool) {
                        continue;
                    }
                    if !scoped.fires_for(call) {
                        continue;
                    }
                    let outcome = h.after_tool(&mut self.state, call, &mut result).await;
                    tracing::debug!(
                        hook_name = h.name(),
                        hook_kind = "write",
                        point = "after_tool",
                        priority = h.priority(),
                        outcome = outcome_kind(&outcome),
                        "hook fired"
                    );
                    self.record_write_hook(run_id, h.name(), HookLifecycle::AfterTool, &outcome);
                    if matches!(outcome, HookOutcome::Stop | HookOutcome::Cancel(_)) {
                        tracing::warn!(run_id, tool = %call.name, hook = h.name(), "hook stopped run after tool");
                        aborted = Some(AgentError::HookStop);
                        break;
                    }
                }
            }
            if aborted.is_none() {
                aborted = self.fire_read_after_tool(run_id, call, &result).await.err();
            }

            if let ToolResult::Deferred { payload } = result {
                self.defer(call, payload);
                continue;
            }

            let (payload, provenance) = Self::persist_result(&result);

            self.emit(crate::AgentEvent::ToolFinished {
                run_id: run_id.to_string(),
                turn,
                call_id: call.id.clone(),
                tool: call.name.clone(),
                status,
                result: match &payload {
                    ToolResultPayload::Inline(value) => value.clone(),
                    artifact => serde_json::Value::String(artifact.text()),
                },
                provenance: provenance.clone(),
                duration_ms,
                at: chrono::Utc::now(),
            });

            blocks.push(ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                tool_name: call.name.clone(),
                content: payload,
                is_error: result.is_error(),
                provenance,
            });
        }

        let errors = blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolResult { is_error: true, .. }))
            .count();
        tracing::Span::current().record("errors", errors);
        tracing::debug!(
            run_id,
            batch_size = blocks.len(),
            errors,
            "tool batch completed"
        );
        if !blocks.is_empty() {
            self.push_tool_results(Message::user_with_blocks(blocks), run_id);
        }
        match aborted {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn defer(&mut self, call: &ToolCall, payload: serde_json::Value) {
        self.state.set_pending(Deferral {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            payload,
        });
    }

    fn persist_result(result: &ToolResult) -> (ToolResultPayload, Vec<ProvenanceSource>) {
        match result {
            ToolResult::Done { output, provenance } => (
                ToolResultPayload::Inline(output.clone()),
                sanitize_provenance(provenance.clone()),
            ),
            ToolResult::Failed { message } => (
                ToolResultPayload::Inline(serde_json::Value::String(message.clone())),
                Vec::new(),
            ),
            ToolResult::Deferred { .. } => (ToolResultPayload::default(), Vec::new()),
        }
    }

    fn tool_context(&self, run_id: &str) -> ToolContext {
        let tool_emitter = std::sync::Arc::new(crate::ToolEmitter {
            sinks: self.state.emitters().to_vec(),
            fold: self.fold_tx.clone(),
        });
        let mut ctx = ToolContext::new(&self.state.user_id, &self.state.session_id, run_id)
            .with_config(self.state.config.clone())
            .with_sub_session(self.sub_session.clone())
            .with_emitter(Some(tool_emitter));
        ctx.insert(crate::TasksSnapshot(std::sync::Arc::new(
            self.state.tasks().clone(),
        )));
        ctx.insert(runic_tool::ActivatedToolNames(std::sync::Arc::new(
            self.activated.names(),
        )));
        ctx
    }

    /// Resolve a tool by name: the static registry first, then the on-demand
    /// activated set (with its unique-suffix fallback).
    fn resolve_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(tool) = self.tools.get(name) {
            return Some(tool.clone());
        }
        self.activated.get_resolved(name)
    }
}

/// Execute one tool with a timeout, mapping every failure mode to an in-band
/// error result the model can read and react to. The monotonic timer wraps
/// exactly this execution — serial batch-mates never inflate each other.
async fn dispatch_one(
    tool: Option<Arc<dyn Tool>>,
    call: ToolCall,
    ctx: ToolContext,
    timeout: Duration,
    parallel: bool,
) -> Dispatched {
    let span = tracing::info_span!(
        "tool",
        gen_ai.tool.name = %call.name,
        gen_ai.tool.call.id = %call.id,
        parallel,
        is_error = tracing::field::Empty,
        outcome = tracing::field::Empty,
        gen_ai.operation.name = "execute_tool",
        otel.name = format!("execute_tool {}", call.name),
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
    );
    let started = std::time::Instant::now();
    let (result, status) = dispatch_one_inner(tool, call, ctx, timeout, &span)
        .instrument(span.clone())
        .await;
    span.record("is_error", result.is_error());
    if result.is_error() {
        span.record("otel.status_code", "ERROR");
    }
    Dispatched {
        result,
        status,
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

async fn dispatch_one_inner(
    tool: Option<Arc<dyn Tool>>,
    call: ToolCall,
    ctx: ToolContext,
    timeout: Duration,
    span: &tracing::Span,
) -> (ToolResult, ToolStatus) {
    let Some(tool) = tool else {
        tracing::warn!(run_id = %ctx.run_id, tool = %call.name, "unknown tool");
        span.record("outcome", "unknown_tool");
        return (
            ToolResult::error(format!("unknown tool: {}", call.name)),
            ToolStatus::UnknownTool,
        );
    };
    // Catch panics so a buggy tool can NEVER abort the run task (which would
    // kill the SSE stream and strand the call). A panic becomes an in-band
    // error result, just like a timeout or a returned `Err`.
    use futures::FutureExt as _;
    let exec = std::panic::AssertUnwindSafe(tool.execute(call.input.clone(), &ctx)).catch_unwind();
    match tokio::time::timeout(timeout, exec).await {
        Ok(Ok(Ok(result))) => {
            if result.is_error() {
                tracing::warn!(
                    run_id = %ctx.run_id,
                    tool = %call.name,
                    error = %result.text(),
                    "tool returned error"
                );
                span.record("outcome", "error");
                (result, ToolStatus::ToolError)
            } else {
                span.record("outcome", "ok");
                (result, ToolStatus::Ok)
            }
        }
        Ok(Ok(Err(e))) => {
            tracing::warn!(run_id = %ctx.run_id, tool = %call.name, error = %e, "tool returned error");
            span.record("outcome", "error");
            (
                ToolResult::error(format!("tool '{}' failed: {e}", call.name)),
                ToolStatus::ExecError,
            )
        }
        Ok(Err(_panic)) => {
            tracing::warn!(run_id = %ctx.run_id, tool = %call.name, "tool panicked");
            span.record("outcome", "panic");
            (
                ToolResult::error(format!("tool '{}' panicked", call.name)),
                ToolStatus::Panic,
            )
        }
        Err(_) => {
            tracing::warn!(run_id = %ctx.run_id, tool = %call.name, timeout_s = timeout.as_secs(), "tool timed out");
            span.record("outcome", "timeout");
            (
                ToolResult::error(format!(
                    "tool '{}' timed out after {}s",
                    call.name,
                    timeout.as_secs()
                )),
                ToolStatus::Timeout,
            )
        }
    }
}
