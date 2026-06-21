//! Step: tool dispatch — runic's 3-phase model.
//!
//! 1. **Plan** (sequential): per call, run the loop guard + `before_tool`
//!    write hooks (which may rewrite the call, substitute a result, cancel, or
//!    stop) + `before_tool` read hooks. Produces a [`CallPlan`] per call.
//! 2. **Execute**: substituted results are filled directly; `parallelizable`
//!    tools run concurrently via `join_all`; the rest run serially. Every
//!    dispatch is timeout-wrapped.
//! 3. **Collect**: `after_tool` hooks fire, and all results are assembled into
//!    a single user-role message appended to the log.

use std::sync::Arc;
use std::time::Duration;

use runic_hook::HookOutcome;
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, Message, ToolCall};

use crate::loop_guard::Verdict;
use crate::{Agent, AgentError};

/// What the loop decided to do with one requested tool call.
enum CallPlan {
    /// Skip execution; this result was supplied by a hook or the guard.
    Substituted { call: ToolCall, result: ToolResult },
    /// Run the tool (possibly concurrently). `warning` is a loop-guard nudge
    /// to append to the result.
    Dispatch {
        call: ToolCall,
        parallelizable: bool,
        warning: Option<String>,
    },
}

impl CallPlan {
    fn call(&self) -> &ToolCall {
        match self {
            CallPlan::Substituted { call, .. } | CallPlan::Dispatch { call, .. } => call,
        }
    }
}

impl Agent {
    /// Drive every tool call the model requested this turn, appending one
    /// combined tool-result message at the end.
    pub(crate) async fn dispatch_tools(
        &mut self,
        calls: Vec<ToolCall>,
        run_id: &str,
    ) -> Result<(), AgentError> {
        // ── Phase 1: plan ──────────────────────────────────────────────────
        let mut plans: Vec<CallPlan> = Vec::with_capacity(calls.len());
        for mut call in calls {
            let guard_warning = match self.guard.check(&call) {
                Verdict::Allow => None,
                Verdict::Warn(msg) => Some(msg),
                Verdict::Block(msg) => {
                    plans.push(CallPlan::Substituted {
                        result: ToolResult::error(msg),
                        call,
                    });
                    continue;
                }
                Verdict::CircuitBreak(msg) => return Err(AgentError::CircuitBreak(msg)),
            };

            let mut substituted: Option<ToolResult> = None;
            for h in self.write_hooks.clone() {
                match h.before_tool(&mut self.state, &mut call).await {
                    HookOutcome::Continue => {}
                    HookOutcome::SubstituteToolResult(r) => {
                        substituted = Some(r);
                        break;
                    }
                    HookOutcome::Cancel(reason) => {
                        substituted = Some(ToolResult::error(reason));
                        break;
                    }
                    HookOutcome::Stop => return Err(AgentError::HookStop),
                }
            }

            self.fire_read_before_tool(&call).await?;

            let plan = match substituted {
                Some(result) => CallPlan::Substituted { call, result },
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
        // dispatch, so they don't get a Started event).
        for plan in &plans {
            if let CallPlan::Dispatch { call, .. } = plan {
                self.emit(crate::AgentEvent::ToolStarted {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                });
            }
        }

        let mut results: Vec<Option<ToolResult>> = (0..plans.len()).map(|_| None).collect();

        // Pre-supplied (hook/guard) results.
        for (i, plan) in plans.iter().enumerate() {
            if let CallPlan::Substituted { result, .. } = plan {
                results[i] = Some(result.clone());
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
                    let ctx = self.tool_context(run_id);
                    async move { (i, dispatch_one(tool, call, ctx, timeout).await) }
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
                let ctx = self.tool_context(run_id);
                results[i] = Some(dispatch_one(tool, call, ctx, timeout).await);
            }
        }

        // ── Phase 3: collect + after_tool hooks ────────────────────────────
        let mut blocks: Vec<ContentBlock> = Vec::with_capacity(plans.len());
        for (i, plan) in plans.iter().enumerate() {
            let call = plan.call();
            let mut result = results[i].take().expect("every plan produced a result");

            // For actually-dispatched calls: feed the outcome to the guard
            // (so identical call+result streaks escalate) and append any nudge.
            if let CallPlan::Dispatch { warning, .. } = plan {
                if let Some(outcome_warning) = self.guard.record_outcome(call, &result.output) {
                    result.output = format!("{}\n\n[loop guard] {outcome_warning}", result.output);
                }
                if let Some(w) = warning {
                    result.output = format!("{}\n\n[loop guard] {w}", result.output);
                }
            }

            if let CallPlan::Dispatch { .. } = plan {
                self.emit(crate::AgentEvent::ToolFinished {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    is_error: !result.success,
                    result: result.output.clone(),
                });
            }

            blocks.push(ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                tool_name: call.name.clone(),
                content: result.output.clone(),
                is_error: !result.success,
            });

            for h in self.write_hooks.clone() {
                if let HookOutcome::Stop = h.after_tool(&mut self.state, call, &result).await {
                    return Err(AgentError::HookStop);
                }
            }
            self.fire_read_after_tool(call, &result).await?;
        }

        self.push_tool_results(Message::user_with_blocks(blocks), run_id);
        Ok(())
    }

    /// A tool context for this run, carrying identity + the per-run config map.
    fn tool_context(&self, run_id: &str) -> ToolContext {
        ToolContext::new(&self.state.user_id, &self.state.session_id, run_id)
            .with_config(self.state.config.clone())
            .with_human(self.human.clone())
    }

    /// Resolve a tool by name: the static registry first, then the on-demand
    /// activated set (with its unique-suffix fallback).
    fn resolve_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(tool) = self.tools.get(name) {
            return Some(tool.clone());
        }
        self.activated.as_ref().and_then(|activated| {
            activated
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get_resolved(name)
        })
    }
}

/// Execute one tool with a timeout, mapping every failure mode to an in-band
/// error result the model can read and react to.
async fn dispatch_one(
    tool: Option<Arc<dyn Tool>>,
    call: ToolCall,
    ctx: ToolContext,
    timeout: Duration,
) -> ToolResult {
    let Some(tool) = tool else {
        return ToolResult::error(format!("unknown tool: {}", call.name));
    };
    // Catch panics so a buggy tool can NEVER abort the run task (which would
    // kill the SSE stream and strand the call). A panic becomes an in-band
    // error result, just like a timeout or a returned `Err`.
    use futures::FutureExt as _;
    let exec = std::panic::AssertUnwindSafe(tool.execute(call.input.clone(), &ctx)).catch_unwind();
    match tokio::time::timeout(timeout, exec).await {
        Ok(Ok(Ok(result))) => result,
        Ok(Ok(Err(e))) => ToolResult::error(format!("tool '{}' failed: {e}", call.name)),
        Ok(Err(_panic)) => ToolResult::error(format!("tool '{}' panicked", call.name)),
        Err(_) => ToolResult::error(format!(
            "tool '{}' timed out after {}s",
            call.name,
            timeout.as_secs()
        )),
    }
}
