use std::collections::HashMap;

use chrono::{DateTime, Utc};
use runic_types::TokenUsage;
use serde::Serialize;

use crate::event::{
    AuditStamp, DelegationMode, DelegationStatus, RunEndStatus, SessionEvent, ToolStatus,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum TraceStatus {
    Completed,
    Failed(String),
    Cancelled,
    InFlight,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunTrace {
    pub run_id: String,
    pub agent: Option<String>,
    pub audit: Option<AuditStamp>,
    pub status: TraceStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub usage: TokenUsage,
    pub turns: Vec<TurnTrace>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnTrace {
    pub turn: u32,
    pub model: Option<String>,
    pub usage: TokenUsage,
    pub model_ms: u64,
    pub ended_at: Option<DateTime<Utc>>,
    pub complete: bool,
    pub tools: Vec<ToolTrace>,
    pub delegations: Vec<DelegationTrace>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolTrace {
    pub call_id: String,
    pub tool: String,
    pub status: Option<ToolStatus>,
    pub duration_ms: Option<u64>,
    pub started_at: Option<DateTime<Utc>>,
    pub deferred: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DelegationTrace {
    pub call_id: String,
    pub agent: String,
    pub mode: Option<DelegationMode>,
    pub status: Option<DelegationStatus>,
    pub usage: TokenUsage,
    pub model: Option<String>,
    pub duration_ms: Option<u64>,
    pub child_session: Option<String>,
}

pub fn project<'a>(events: impl IntoIterator<Item = &'a SessionEvent>) -> Vec<RunTrace> {
    let mut runs: Vec<RunTrace> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    fn run_at<'r>(
        runs: &'r mut Vec<RunTrace>,
        index: &mut HashMap<String, usize>,
        run_id: &str,
    ) -> &'r mut RunTrace {
        let i = *index.entry(run_id.to_string()).or_insert_with(|| {
            runs.push(RunTrace {
                run_id: run_id.to_string(),
                agent: None,
                audit: None,
                status: TraceStatus::InFlight,
                started_at: None,
                ended_at: None,
                usage: TokenUsage::default(),
                turns: Vec::new(),
            });
            runs.len() - 1
        });
        &mut runs[i]
    }

    fn turn_at(run: &mut RunTrace, turn: u32) -> &mut TurnTrace {
        let pos = match run.turns.iter().position(|t| t.turn == turn) {
            Some(pos) => pos,
            None => {
                let at = run.turns.iter().position(|t| t.turn > turn);
                let trace = TurnTrace {
                    turn,
                    model: None,
                    usage: TokenUsage::default(),
                    model_ms: 0,
                    ended_at: None,
                    complete: false,
                    tools: Vec::new(),
                    delegations: Vec::new(),
                };
                match at {
                    Some(at) => {
                        run.turns.insert(at, trace);
                        at
                    }
                    None => {
                        run.turns.push(trace);
                        run.turns.len() - 1
                    }
                }
            }
        };
        &mut run.turns[pos]
    }

    for event in events {
        match event {
            SessionEvent::RunStart {
                run_id,
                agent,
                audit,
                at,
            } => {
                let run = run_at(&mut runs, &mut index, run_id);
                if run.started_at.is_none() {
                    run.started_at = Some(*at);
                    run.agent = agent.clone();
                    run.audit = audit.clone();
                }
            }
            SessionEvent::RunEnd {
                run_id, status, at, ..
            } => {
                let run = run_at(&mut runs, &mut index, run_id);
                if run.ended_at.is_none() {
                    run.ended_at = Some(*at);
                    run.status = match status {
                        RunEndStatus::Completed => TraceStatus::Completed,
                        RunEndStatus::Failed(e) => TraceStatus::Failed(e.clone()),
                        RunEndStatus::Cancelled => TraceStatus::Cancelled,
                    };
                }
            }
            SessionEvent::TurnEnd {
                run_id,
                turn,
                model,
                usage,
                model_ms,
                at,
            } => {
                let run = run_at(&mut runs, &mut index, run_id);
                let trace = turn_at(run, *turn);
                if !trace.complete {
                    trace.complete = true;
                    trace.model = Some(model.clone());
                    trace.usage = *usage;
                    trace.model_ms = *model_ms;
                    trace.ended_at = Some(*at);
                    run.usage.add(usage);
                }
            }
            SessionEvent::ToolStarted {
                run_id,
                turn,
                call_id,
                tool,
                at,
            } => {
                let run = run_at(&mut runs, &mut index, run_id);
                let trace = turn_at(run, *turn);
                match trace
                    .tools
                    .iter_mut()
                    .find(|t| t.call_id == *call_id && t.tool == *tool)
                {
                    Some(existing) => {
                        if existing.started_at.is_none() {
                            existing.started_at = Some(*at);
                        }
                    }
                    None => trace.tools.push(ToolTrace {
                        call_id: call_id.clone(),
                        tool: tool.clone(),
                        status: None,
                        duration_ms: None,
                        started_at: Some(*at),
                        deferred: false,
                    }),
                }
            }
            SessionEvent::ToolFinished {
                run_id,
                turn,
                call_id,
                tool,
                status,
                duration_ms,
                ..
            } => {
                let run = run_at(&mut runs, &mut index, run_id);
                let trace = turn_at(run, *turn);
                match trace
                    .tools
                    .iter_mut()
                    .find(|t| t.call_id == *call_id && t.tool == *tool)
                {
                    Some(existing) => {
                        if existing.status.is_none() {
                            existing.status = Some(*status);
                            existing.duration_ms = Some(*duration_ms);
                        }
                    }
                    None => trace.tools.push(ToolTrace {
                        call_id: call_id.clone(),
                        tool: tool.clone(),
                        status: Some(*status),
                        duration_ms: Some(*duration_ms),
                        started_at: None,
                        deferred: false,
                    }),
                }
            }
            SessionEvent::ToolDeferred {
                run_id, call_id, ..
            } => {
                let run = run_at(&mut runs, &mut index, run_id);
                for turn in &mut run.turns {
                    if let Some(tool) = turn.tools.iter_mut().find(|t| t.call_id == *call_id) {
                        tool.deferred = true;
                    }
                }
            }
            SessionEvent::DelegationStarted {
                run_id,
                turn,
                call_id,
                agent,
                mode,
                child_session,
                ..
            } => {
                let run = run_at(&mut runs, &mut index, run_id);
                let trace = turn_at(run, *turn);
                match trace
                    .delegations
                    .iter_mut()
                    .find(|d| d.call_id == *call_id && d.agent == *agent)
                {
                    Some(existing) => {
                        if existing.mode.is_none() {
                            existing.mode = Some(*mode);
                            existing.child_session = child_session.clone();
                        }
                    }
                    None => trace.delegations.push(DelegationTrace {
                        call_id: call_id.clone(),
                        agent: agent.clone(),
                        mode: Some(*mode),
                        status: None,
                        usage: TokenUsage::default(),
                        model: None,
                        duration_ms: None,
                        child_session: child_session.clone(),
                    }),
                }
            }
            SessionEvent::DelegationFinished {
                run_id,
                turn,
                call_id,
                agent,
                status,
                usage,
                model,
                duration_ms,
                ..
            } => {
                let run = run_at(&mut runs, &mut index, run_id);
                let trace = turn_at(run, *turn);
                match trace
                    .delegations
                    .iter_mut()
                    .find(|d| d.call_id == *call_id && d.agent == *agent)
                {
                    Some(existing) => {
                        if existing.status.is_none() {
                            existing.status = Some(status.clone());
                            existing.usage = *usage;
                            existing.model = model.clone();
                            existing.duration_ms = Some(*duration_ms);
                        }
                    }
                    None => trace.delegations.push(DelegationTrace {
                        call_id: call_id.clone(),
                        agent: agent.clone(),
                        mode: None,
                        status: Some(status.clone()),
                        usage: *usage,
                        model: model.clone(),
                        duration_ms: Some(*duration_ms),
                        child_session: None,
                    }),
                }
            }
            _ => {}
        }
    }
    runs
}
