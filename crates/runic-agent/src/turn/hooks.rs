//! Hook fan-out (the two-trait seam). `WriteHook`s run **sequentially** by
//! priority with `&mut AgentState` and a rich `HookOutcome`; `ReadHook`s run
//! **in parallel** with `&AgentState` and may only `Continue`/`Stop`.

use chrono::Utc;
use runic_hook::{HookOutcome, HookSignal};
use runic_state::{HookLifecycle, SessionEvent};
use runic_tool::ToolResult;
use runic_types::ToolCall;

use crate::{Agent, AgentError, AgentEvent};

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

    fn lifecycle(self) -> HookLifecycle {
        match self {
            Point::BeforeAgent => HookLifecycle::BeforeAgent,
            Point::AfterAgent => HookLifecycle::AfterAgent,
            Point::BeforeModel => HookLifecycle::BeforeModel,
            Point::AfterModel => HookLifecycle::AfterModel,
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
    pub(super) fn record_write_hook(
        &mut self,
        run_id: &str,
        hook_name: &str,
        lifecycle: HookLifecycle,
        outcome: &HookOutcome,
    ) {
        let kind = outcome_kind(outcome);
        let note = match outcome {
            HookOutcome::Cancel(reason) => Some(reason.clone()),
            _ => None,
        };
        self.state.push_event(SessionEvent::HookRan {
            run_id: run_id.to_string(),
            hook: hook_name.to_string(),
            lifecycle,
            hook_kind: "write".to_string(),
            outcome: kind.to_string(),
            note: note.clone(),
            at: Utc::now(),
        });
        self.emit(AgentEvent::HookFired {
            hook_name: hook_name.to_string(),
            hook_kind: "write",
            lifecycle,
            outcome: kind,
            note,
        });
    }

    fn record_read_hook_stop(&mut self, run_id: &str, hook_name: &str, lifecycle: HookLifecycle) {
        self.state.push_event(SessionEvent::HookRan {
            run_id: run_id.to_string(),
            hook: hook_name.to_string(),
            lifecycle,
            hook_kind: "read".to_string(),
            outcome: "stop".to_string(),
            note: None,
            at: Utc::now(),
        });
        self.emit(AgentEvent::HookFired {
            hook_name: hook_name.to_string(),
            hook_kind: "read",
            lifecycle,
            outcome: "stop",
            note: None,
        });
    }

    pub(crate) async fn fire_write(
        &mut self,
        run_id: &str,
        point: Point,
    ) -> Result<(), AgentError> {
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
            if !matches!(outcome, HookOutcome::Continue) {
                self.record_write_hook(run_id, h.name(), point.lifecycle(), &outcome);
            }
            match outcome {
                HookOutcome::Continue | HookOutcome::SubstituteToolResult(_) => {}
                HookOutcome::Cancel(_) | HookOutcome::Stop => return Err(AgentError::HookStop),
            }
        }
        Ok(())
    }

    pub(crate) async fn fire_read(&mut self, run_id: &str, point: Point) -> Result<(), AgentError> {
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
                    (h.name().to_string(), signal)
                }
            })
            .collect();
        let results = futures::future::join_all(futs).await;
        for (hook_name, signal) in &results {
            if matches!(signal, HookSignal::Stop) {
                self.record_read_hook_stop(run_id, hook_name, point.lifecycle());
            }
        }
        signals_ok(results.into_iter().map(|(_, s)| s).collect())
    }

    pub(crate) async fn fire_read_before_tool(
        &mut self,
        run_id: &str,
        call: &ToolCall,
    ) -> Result<(), AgentError> {
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
                    (h.name().to_string(), signal)
                }
            })
            .collect();
        let results = futures::future::join_all(futs).await;
        for (hook_name, signal) in &results {
            if matches!(signal, HookSignal::Stop) {
                self.record_read_hook_stop(run_id, hook_name, HookLifecycle::BeforeTool);
            }
        }
        signals_ok(results.into_iter().map(|(_, s)| s).collect())
    }

    pub(crate) async fn fire_read_after_tool(
        &mut self,
        run_id: &str,
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
                    (h.name().to_string(), signal)
                }
            })
            .collect();
        let results = futures::future::join_all(futs).await;
        for (hook_name, signal) in &results {
            if matches!(signal, HookSignal::Stop) {
                self.record_read_hook_stop(run_id, hook_name, HookLifecycle::AfterTool);
            }
        }
        signals_ok(results.into_iter().map(|(_, s)| s).collect())
    }
}

fn signals_ok(signals: Vec<HookSignal>) -> Result<(), AgentError> {
    if signals.iter().any(|s| matches!(s, HookSignal::Stop)) {
        Err(AgentError::HookStop)
    } else {
        Ok(())
    }
}
