//! Hook fan-out (the two-trait seam). `WriteHook`s run **sequentially** by
//! priority with `&mut AgentState` and a rich `HookOutcome`; `ReadHook`s run
//! **in parallel** with `&AgentState` and may only `Continue`/`Stop`.

use chrono::Utc;
use runic_hook::{HookOutcome, HookSignal};
use runic_state::HookLifecycle;
use runic_tool::ToolResult;
use runic_types::ToolCall;

use crate::{AgentError, AgentEvent, Runner};

/// The non-tool lifecycle points. Tool points (`before_tool`/`after_tool`)
/// fire inside [`Runner::dispatch_tools`] because they carry the call/result.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Point {
    BeforeAgent,
    AfterAgent,
}

impl Point {
    fn as_str(self) -> &'static str {
        match self {
            Point::BeforeAgent => "before_agent",
            Point::AfterAgent => "after_agent",
        }
    }

    fn lifecycle(self) -> HookLifecycle {
        match self {
            Point::BeforeAgent => HookLifecycle::BeforeAgent,
            Point::AfterAgent => HookLifecycle::AfterAgent,
        }
    }
}

pub(super) fn outcome_kind(outcome: &HookOutcome) -> &'static str {
    match outcome {
        HookOutcome::Noop => "noop",
        HookOutcome::Continue => "continue",
        HookOutcome::SubstituteToolResult(_) => "substitute",
        HookOutcome::Cancel(_) => "cancel",
        HookOutcome::Stop => "stop",
    }
}

fn signal_kind(signal: &HookSignal) -> &'static str {
    match signal {
        HookSignal::Noop => "noop",
        HookSignal::Continue => "continue",
        HookSignal::Stop => "stop",
    }
}

impl Runner {
    pub(super) fn record_write_hook(
        &mut self,
        run_id: &str,
        hook_name: &str,
        lifecycle: HookLifecycle,
        outcome: &HookOutcome,
    ) {
        if matches!(outcome, HookOutcome::Noop) {
            return;
        }
        let kind = outcome_kind(outcome);
        let note = match outcome {
            HookOutcome::Cancel(reason) => Some(reason.clone()),
            _ => None,
        };
        self.emit(AgentEvent::HookFired {
            run_id: run_id.to_string(),
            hook: hook_name.to_string(),
            hook_kind: "write".to_string(),
            lifecycle,
            outcome: kind.to_string(),
            note,
            at: Utc::now(),
        });
    }

    fn record_read_hook(
        &mut self,
        run_id: &str,
        hook_name: &str,
        lifecycle: HookLifecycle,
        signal: &HookSignal,
    ) {
        if matches!(signal, HookSignal::Noop) {
            return;
        }
        let kind = signal_kind(signal);
        self.emit(AgentEvent::HookFired {
            run_id: run_id.to_string(),
            hook: hook_name.to_string(),
            hook_kind: "read".to_string(),
            lifecycle,
            outcome: kind.to_string(),
            note: None,
            at: Utc::now(),
        });
    }

    pub(crate) async fn fire_write_before_model(
        &mut self,
        run_id: &str,
        request: &mut runic_provider::CompletionRequest,
    ) -> Result<(), AgentError> {
        for scoped in self.write_hooks.clone() {
            let h = &scoped.hook;
            if !h.points().contains(&HookLifecycle::BeforeModel) {
                continue;
            }
            if !scoped.fires_this_turn() {
                continue;
            }
            let outcome = h.before_model(&mut self.state, request).await;
            tracing::debug!(
                hook_name = h.name(),
                hook_kind = "write",
                point = "before_model",
                priority = h.priority(),
                outcome = outcome_kind(&outcome),
                "hook fired"
            );
            self.record_write_hook(run_id, h.name(), HookLifecycle::BeforeModel, &outcome);
            match outcome {
                HookOutcome::Noop
                | HookOutcome::Continue
                | HookOutcome::SubstituteToolResult(_) => {}
                HookOutcome::Stop | HookOutcome::Cancel(_) => return Err(AgentError::HookStop),
            }
        }
        Ok(())
    }

    pub(crate) async fn fire_read_before_model(
        &mut self,
        run_id: &str,
        request: &runic_provider::CompletionRequest,
    ) -> Result<(), AgentError> {
        for h in self.read_hooks.clone() {
            if !h.points().contains(&HookLifecycle::BeforeModel) {
                continue;
            }
            let signal = h.before_model(&self.state, request).await;
            tracing::debug!(
                hook_name = h.name(),
                hook_kind = "read",
                point = "before_model",
                priority = h.priority(),
                "hook fired"
            );
            self.record_read_hook(run_id, h.name(), HookLifecycle::BeforeModel, &signal);
            if matches!(signal, HookSignal::Stop) {
                return Err(AgentError::HookStop);
            }
        }
        Ok(())
    }

    pub(crate) async fn fire_write_after_model(
        &mut self,
        run_id: &str,
        response: &mut runic_provider::CompletionResponse,
    ) -> Result<(), AgentError> {
        for scoped in self.write_hooks.clone() {
            let h = &scoped.hook;
            if !h.points().contains(&HookLifecycle::AfterModel) {
                continue;
            }
            if !scoped.fires_this_turn() {
                continue;
            }
            let outcome = h.after_model(&mut self.state, response).await;
            tracing::debug!(
                hook_name = h.name(),
                hook_kind = "write",
                point = "after_model",
                priority = h.priority(),
                outcome = outcome_kind(&outcome),
                "hook fired"
            );
            self.record_write_hook(run_id, h.name(), HookLifecycle::AfterModel, &outcome);
            match outcome {
                HookOutcome::Noop
                | HookOutcome::Continue
                | HookOutcome::SubstituteToolResult(_) => {}
                HookOutcome::Stop | HookOutcome::Cancel(_) => return Err(AgentError::HookStop),
            }
        }
        Ok(())
    }

    pub(crate) async fn fire_read_after_model(
        &mut self,
        run_id: &str,
        response: &runic_provider::CompletionResponse,
    ) -> Result<(), AgentError> {
        for h in self.read_hooks.clone() {
            if !h.points().contains(&HookLifecycle::AfterModel) {
                continue;
            }
            let signal = h.after_model(&self.state, response).await;
            tracing::debug!(
                hook_name = h.name(),
                hook_kind = "read",
                point = "after_model",
                priority = h.priority(),
                "hook fired"
            );
            self.record_read_hook(run_id, h.name(), HookLifecycle::AfterModel, &signal);
            if matches!(signal, HookSignal::Stop) {
                return Err(AgentError::HookStop);
            }
        }
        Ok(())
    }

    pub(crate) async fn fire_write(
        &mut self,
        run_id: &str,
        point: Point,
    ) -> Result<(), AgentError> {
        for scoped in self.write_hooks.clone() {
            let h = &scoped.hook;
            if !h.points().contains(&point.lifecycle()) {
                continue;
            }
            if !scoped.fires_this_turn() {
                continue;
            }
            let outcome = match point {
                Point::BeforeAgent => h.before_agent(&mut self.state).await,
                Point::AfterAgent => h.after_agent(&mut self.state).await,
            };
            tracing::debug!(
                hook_name = h.name(),
                hook_kind = "write",
                point = point.as_str(),
                priority = h.priority(),
                outcome = outcome_kind(&outcome),
                "hook fired"
            );
            self.record_write_hook(run_id, h.name(), point.lifecycle(), &outcome);
            match outcome {
                HookOutcome::Noop
                | HookOutcome::Continue
                | HookOutcome::SubstituteToolResult(_) => {}
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
            .filter(|h| h.points().contains(&point.lifecycle()))
            .map(|h| {
                let h = h.clone();
                async move {
                    let signal = match point {
                        Point::BeforeAgent => h.before_agent(state).await,
                        Point::AfterAgent => h.after_agent(state).await,
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
            self.record_read_hook(run_id, hook_name, point.lifecycle(), signal);
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
            .filter(|h| h.points().contains(&HookLifecycle::BeforeTool))
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
            self.record_read_hook(run_id, hook_name, HookLifecycle::BeforeTool, signal);
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
            .filter(|h| h.points().contains(&HookLifecycle::AfterTool))
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
            self.record_read_hook(run_id, hook_name, HookLifecycle::AfterTool, signal);
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
