//! Hook fan-out (the two-trait seam). `WriteHook`s run **sequentially** by
//! priority with `&mut AgentState` and a rich `HookOutcome`; `ReadHook`s run
//! **in parallel** with `&AgentState` and may only `Continue`/`Stop`.

use runic_hook::{HookOutcome, HookSignal};
use runic_tool::ToolResult;
use runic_types::ToolCall;

use crate::{Agent, AgentError};

/// The non-tool lifecycle points. Tool points (`before_tool`/`after_tool`)
/// fire inside [`Agent::dispatch_tools`] because they carry the call/result.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Point {
    BeforeAgent,
    AfterAgent,
    BeforeModel,
    AfterModel,
}

impl Point {
    fn as_str(self) -> &'static str {
        match self {
            Point::BeforeAgent => "before_agent",
            Point::AfterAgent => "after_agent",
            Point::BeforeModel => "before_model",
            Point::AfterModel => "after_model",
        }
    }
}

pub(super) fn outcome_kind(outcome: &HookOutcome) -> &'static str {
    match outcome {
        HookOutcome::Continue => "continue",
        HookOutcome::SubstituteToolResult(_) => "substitute",
        HookOutcome::Cancel(_) => "cancel",
        HookOutcome::Stop => "stop",
    }
}

fn signal_kind(signal: &HookSignal) -> &'static str {
    match signal {
        HookSignal::Continue => "continue",
        HookSignal::Stop => "stop",
    }
}

impl Agent {
    /// Fire write hooks at a lifecycle point, sequentially. `Cancel`/`Stop`
    /// halt the run; `SubstituteToolResult` is meaningless here and ignored.
    pub(crate) async fn fire_write(&mut self, point: Point) -> Result<(), AgentError> {
        for h in self.write_hooks.clone() {
            let outcome = match point {
                Point::BeforeAgent => h.before_agent(&mut self.state).await,
                Point::AfterAgent => h.after_agent(&mut self.state).await,
                Point::BeforeModel => h.before_model(&mut self.state).await,
                Point::AfterModel => h.after_model(&mut self.state).await,
            };
            tracing::debug!(
                hook_name = h.name(),
                hook_kind = "write",
                point = point.as_str(),
                priority = h.priority(),
                outcome = outcome_kind(&outcome),
                "hook fired"
            );
            match outcome {
                HookOutcome::Continue | HookOutcome::SubstituteToolResult(_) => {}
                HookOutcome::Cancel(_) | HookOutcome::Stop => return Err(AgentError::HookStop),
            }
        }
        Ok(())
    }

    /// Fire read hooks at a lifecycle point, in parallel. Any `Stop` halts.
    pub(crate) async fn fire_read(&self, point: Point) -> Result<(), AgentError> {
        let hooks = self.read_hooks.clone();
        let state = &self.state;
        let futs: Vec<_> = hooks
            .iter()
            .map(|h| {
                let h = h.clone();
                async move {
                    let signal = match point {
                        Point::BeforeAgent => h.before_agent(state).await,
                        Point::AfterAgent => h.after_agent(state).await,
                        Point::BeforeModel => h.before_model(state).await,
                        Point::AfterModel => h.after_model(state).await,
                    };
                    tracing::debug!(
                        hook_name = h.name(),
                        hook_kind = "read",
                        point = point.as_str(),
                        priority = h.priority(),
                        outcome = signal_kind(&signal),
                        "hook fired"
                    );
                    signal
                }
            })
            .collect();
        signals_ok(futures::future::join_all(futs).await)
    }

    /// Read hooks observing a (final, post-write-hook) tool call before dispatch.
    pub(crate) async fn fire_read_before_tool(&self, call: &ToolCall) -> Result<(), AgentError> {
        let hooks = self.read_hooks.clone();
        let state = &self.state;
        let futs: Vec<_> = hooks
            .iter()
            .map(|h| {
                let h = h.clone();
                async move {
                    let signal = h.before_tool(state, call).await;
                    tracing::debug!(
                        hook_name = h.name(),
                        hook_kind = "read",
                        point = "before_tool",
                        priority = h.priority(),
                        outcome = signal_kind(&signal),
                        "hook fired"
                    );
                    signal
                }
            })
            .collect();
        signals_ok(futures::future::join_all(futs).await)
    }

    /// Read hooks observing a tool result after dispatch.
    pub(crate) async fn fire_read_after_tool(
        &self,
        call: &ToolCall,
        result: &ToolResult,
    ) -> Result<(), AgentError> {
        let hooks = self.read_hooks.clone();
        let state = &self.state;
        let futs: Vec<_> = hooks
            .iter()
            .map(|h| {
                let h = h.clone();
                async move {
                    let signal = h.after_tool(state, call, result).await;
                    tracing::debug!(
                        hook_name = h.name(),
                        hook_kind = "read",
                        point = "after_tool",
                        priority = h.priority(),
                        outcome = signal_kind(&signal),
                        "hook fired"
                    );
                    signal
                }
            })
            .collect();
        signals_ok(futures::future::join_all(futs).await)
    }
}

fn signals_ok(signals: Vec<HookSignal>) -> Result<(), AgentError> {
    if signals.iter().any(|s| matches!(s, HookSignal::Stop)) {
        Err(AgentError::HookStop)
    } else {
        Ok(())
    }
}
