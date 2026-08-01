use chrono::{DateTime, Utc};
use runic_state::{
    AgentEvent, AuditStamp, DelegationMode, DelegationStatus, HookLifecycle, PersistenceStatus,
    RunEndStatus, RunOutcome, SessionStats, TaskRecord, TaskStatus, ToolStatus,
};
use runic_types::{Message, TokenUsage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SessionEvent {
    RunStart {
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audit: Option<AuditStamp>,
        at: DateTime<Utc>,
    },

    RunEnd {
        run_id: String,
        status: RunEndStatus,
        outcome: RunOutcome,
        at: DateTime<Utc>,
    },

    Message {
        run_id: String,
        msg: Message,
        at: DateTime<Utc>,
    },

    TurnEnd {
        run_id: String,
        turn: u32,
        model: String,
        usage: TokenUsage,
        model_ms: u64,
        at: DateTime<Utc>,
    },

    ToolStarted {
        run_id: String,
        turn: u32,
        call_id: String,
        tool: String,
        at: DateTime<Utc>,
    },

    ToolFinished {
        run_id: String,
        turn: u32,
        call_id: String,
        tool: String,
        status: ToolStatus,
        duration_ms: u64,
        at: DateTime<Utc>,
    },

    DelegationStarted {
        run_id: String,
        turn: u32,
        call_id: String,
        agent: String,
        mode: DelegationMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_session: Option<String>,
        at: DateTime<Utc>,
    },

    DelegationFinished {
        run_id: String,
        turn: u32,
        call_id: String,
        agent: String,
        status: DelegationStatus,
        usage: TokenUsage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_persistence: Option<PersistenceStatus>,
        at: DateTime<Utc>,
    },

    #[serde(alias = "HookRan")]
    HookFired {
        run_id: String,
        hook: String,
        lifecycle: HookLifecycle,
        #[serde(default)]
        hook_kind: String,
        #[serde(default)]
        outcome: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        at: DateTime<Utc>,
    },

    StateSnapshot {
        run_id: String,
        messages: Vec<Message>,
        system_prompt: String,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stats: Option<Box<SessionStats>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        open_tasks: Option<Vec<TaskRecord>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Map<String, serde_json::Value>>,
        at: DateTime<Utc>,
    },

    TaskSpawned {
        run_id: String,
        task_id: String,
        agent: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child_session: Option<String>,
        at: DateTime<Utc>,
    },

    TaskFinished {
        run_id: String,
        task_id: String,
        status: TaskStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        at: DateTime<Utc>,
    },

    StateUpdated {
        run_id: String,
        key: String,
        value: serde_json::Value,
        at: DateTime<Utc>,
    },

    ToolDeferred {
        run_id: String,
        call_id: String,
        tool: String,
        payload: serde_json::Value,
        at: DateTime<Utc>,
    },
}

impl SessionEvent {
    pub fn run_id(&self) -> &str {
        match self {
            SessionEvent::RunStart { run_id, .. }
            | SessionEvent::RunEnd { run_id, .. }
            | SessionEvent::Message { run_id, .. }
            | SessionEvent::TurnEnd { run_id, .. }
            | SessionEvent::ToolStarted { run_id, .. }
            | SessionEvent::ToolFinished { run_id, .. }
            | SessionEvent::DelegationStarted { run_id, .. }
            | SessionEvent::DelegationFinished { run_id, .. }
            | SessionEvent::HookFired { run_id, .. }
            | SessionEvent::StateSnapshot { run_id, .. }
            | SessionEvent::TaskSpawned { run_id, .. }
            | SessionEvent::TaskFinished { run_id, .. }
            | SessionEvent::StateUpdated { run_id, .. }
            | SessionEvent::ToolDeferred { run_id, .. } => run_id,
        }
    }

    pub fn lift(&self) -> AgentEvent {
        match self {
            SessionEvent::RunStart {
                run_id,
                agent,
                audit,
                at,
            } => AgentEvent::RunStarted {
                run_id: run_id.clone(),
                agent: agent.clone(),
                audit: audit.clone(),
                at: *at,
            },
            SessionEvent::RunEnd {
                run_id,
                status,
                outcome,
                at,
            } => AgentEvent::RunEnd {
                run_id: run_id.clone(),
                status: status.clone(),
                outcome: outcome.clone(),
                at: *at,
            },
            SessionEvent::Message { run_id, msg, at } => AgentEvent::Message {
                run_id: run_id.clone(),
                msg: msg.clone(),
                at: *at,
            },
            SessionEvent::TurnEnd {
                run_id,
                turn,
                model,
                usage,
                model_ms,
                at,
            } => AgentEvent::TurnEnd {
                run_id: run_id.clone(),
                turn: *turn,
                model: model.clone(),
                usage: *usage,
                model_ms: *model_ms,
                stop_reason: String::new(),
                at: *at,
            },
            SessionEvent::ToolStarted {
                run_id,
                turn,
                call_id,
                tool,
                at,
            } => AgentEvent::ToolStarted {
                run_id: run_id.clone(),
                turn: *turn,
                call_id: call_id.clone(),
                tool: tool.clone(),
                input: serde_json::Value::Null,
                at: *at,
            },
            SessionEvent::ToolFinished {
                run_id,
                turn,
                call_id,
                tool,
                status,
                duration_ms,
                at,
            } => AgentEvent::ToolFinished {
                run_id: run_id.clone(),
                turn: *turn,
                call_id: call_id.clone(),
                tool: tool.clone(),
                status: *status,
                result: serde_json::Value::Null,
                provenance: Vec::new(),
                duration_ms: *duration_ms,
                at: *at,
            },
            SessionEvent::DelegationStarted {
                run_id,
                turn,
                call_id,
                agent,
                mode,
                child_session,
                at,
            } => AgentEvent::DelegationStarted {
                run_id: run_id.clone(),
                turn: *turn,
                call_id: call_id.clone(),
                agent: agent.clone(),
                mode: *mode,
                child_session: child_session.clone(),
                at: *at,
            },
            SessionEvent::DelegationFinished {
                run_id,
                turn,
                call_id,
                agent,
                status,
                usage,
                model,
                duration_ms,
                child_session,
                child_persistence,
                at,
            } => AgentEvent::DelegationFinished {
                run_id: run_id.clone(),
                turn: *turn,
                call_id: call_id.clone(),
                agent: agent.clone(),
                status: status.clone(),
                usage: *usage,
                model: model.clone(),
                duration_ms: *duration_ms,
                child_session: child_session.clone(),
                child_persistence: child_persistence.clone(),
                at: *at,
            },
            SessionEvent::HookFired {
                run_id,
                hook,
                lifecycle,
                hook_kind,
                outcome,
                note,
                at,
            } => AgentEvent::HookFired {
                run_id: run_id.clone(),
                hook: hook.clone(),
                hook_kind: hook_kind.clone(),
                lifecycle: *lifecycle,
                outcome: outcome.clone(),
                note: note.clone(),
                at: *at,
            },
            SessionEvent::StateSnapshot {
                run_id,
                messages,
                system_prompt,
                reason,
                stats,
                open_tasks,
                data,
                at,
            } => AgentEvent::StateSnapshot {
                run_id: run_id.clone(),
                messages: messages.clone(),
                system_prompt: system_prompt.clone(),
                reason: reason.clone(),
                stats: stats.clone(),
                open_tasks: open_tasks.clone(),
                data: data.clone(),
                at: *at,
            },
            SessionEvent::TaskSpawned {
                run_id,
                task_id,
                agent,
                prompt,
                child_session,
                at,
            } => AgentEvent::TaskSpawned {
                run_id: run_id.clone(),
                task_id: task_id.clone(),
                agent: agent.clone(),
                prompt: prompt.clone(),
                child_session: child_session.clone(),
                at: *at,
            },
            SessionEvent::TaskFinished {
                run_id,
                task_id,
                status,
                result,
                at,
            } => AgentEvent::TaskFinished {
                run_id: run_id.clone(),
                task_id: task_id.clone(),
                status: *status,
                result: result.clone(),
                at: *at,
            },
            SessionEvent::StateUpdated {
                run_id,
                key,
                value,
                at,
            } => AgentEvent::StateUpdated {
                run_id: run_id.clone(),
                key: key.clone(),
                value: value.clone(),
                at: *at,
            },
            SessionEvent::ToolDeferred {
                run_id,
                call_id,
                tool,
                payload,
                at,
            } => AgentEvent::ToolDeferred {
                run_id: run_id.clone(),
                call_id: call_id.clone(),
                tool: tool.clone(),
                payload: payload.clone(),
                at: *at,
            },
        }
    }
}

pub fn project(event: &AgentEvent) -> Option<SessionEvent> {
    Some(match event {
        AgentEvent::TextDelta(_) | AgentEvent::ThinkingDelta(_) | AgentEvent::Persisted { .. } => {
            return None;
        }
        AgentEvent::RunStarted {
            run_id,
            agent,
            audit,
            at,
        } => SessionEvent::RunStart {
            run_id: run_id.clone(),
            agent: agent.clone(),
            audit: audit.clone(),
            at: *at,
        },
        AgentEvent::Message { run_id, msg, at } => SessionEvent::Message {
            run_id: run_id.clone(),
            msg: msg.clone(),
            at: *at,
        },
        AgentEvent::ToolStarted {
            run_id,
            turn,
            call_id,
            tool,
            at,
            ..
        } => SessionEvent::ToolStarted {
            run_id: run_id.clone(),
            turn: *turn,
            call_id: call_id.clone(),
            tool: tool.clone(),
            at: *at,
        },
        AgentEvent::ToolFinished {
            run_id,
            turn,
            call_id,
            tool,
            status,
            duration_ms,
            at,
            ..
        } => SessionEvent::ToolFinished {
            run_id: run_id.clone(),
            turn: *turn,
            call_id: call_id.clone(),
            tool: tool.clone(),
            status: *status,
            duration_ms: *duration_ms,
            at: *at,
        },
        AgentEvent::TurnEnd {
            run_id,
            turn,
            model,
            usage,
            model_ms,
            at,
            ..
        } => SessionEvent::TurnEnd {
            run_id: run_id.clone(),
            turn: *turn,
            model: model.clone(),
            usage: *usage,
            model_ms: *model_ms,
            at: *at,
        },
        AgentEvent::ToolDeferred {
            run_id,
            call_id,
            tool,
            payload,
            at,
        } => SessionEvent::ToolDeferred {
            run_id: run_id.clone(),
            call_id: call_id.clone(),
            tool: tool.clone(),
            payload: payload.clone(),
            at: *at,
        },
        AgentEvent::HookFired {
            run_id,
            hook,
            hook_kind,
            lifecycle,
            outcome,
            note,
            at,
        } => SessionEvent::HookFired {
            run_id: run_id.clone(),
            hook: hook.clone(),
            lifecycle: *lifecycle,
            hook_kind: hook_kind.clone(),
            outcome: outcome.clone(),
            note: note.clone(),
            at: *at,
        },
        AgentEvent::DelegationStarted {
            run_id,
            turn,
            call_id,
            agent,
            mode,
            child_session,
            at,
        } => SessionEvent::DelegationStarted {
            run_id: run_id.clone(),
            turn: *turn,
            call_id: call_id.clone(),
            agent: agent.clone(),
            mode: *mode,
            child_session: child_session.clone(),
            at: *at,
        },
        AgentEvent::DelegationFinished {
            run_id,
            turn,
            call_id,
            agent,
            status,
            usage,
            model,
            duration_ms,
            child_session,
            child_persistence,
            at,
        } => SessionEvent::DelegationFinished {
            run_id: run_id.clone(),
            turn: *turn,
            call_id: call_id.clone(),
            agent: agent.clone(),
            status: status.clone(),
            usage: *usage,
            model: model.clone(),
            duration_ms: *duration_ms,
            child_session: child_session.clone(),
            child_persistence: child_persistence.clone(),
            at: *at,
        },
        AgentEvent::StateSnapshot {
            run_id,
            messages,
            system_prompt,
            reason,
            stats,
            open_tasks,
            data,
            at,
        } => SessionEvent::StateSnapshot {
            run_id: run_id.clone(),
            messages: messages.clone(),
            system_prompt: system_prompt.clone(),
            reason: reason.clone(),
            stats: stats.clone(),
            open_tasks: open_tasks.clone(),
            data: data.clone(),
            at: *at,
        },
        AgentEvent::StateUpdated {
            run_id,
            key,
            value,
            at,
        } => SessionEvent::StateUpdated {
            run_id: run_id.clone(),
            key: key.clone(),
            value: value.clone(),
            at: *at,
        },
        AgentEvent::TaskSpawned {
            run_id,
            task_id,
            agent,
            prompt,
            child_session,
            at,
        } => SessionEvent::TaskSpawned {
            run_id: run_id.clone(),
            task_id: task_id.clone(),
            agent: agent.clone(),
            prompt: prompt.clone(),
            child_session: child_session.clone(),
            at: *at,
        },
        AgentEvent::TaskFinished {
            run_id,
            task_id,
            status,
            result,
            at,
        } => SessionEvent::TaskFinished {
            run_id: run_id.clone(),
            task_id: task_id.clone(),
            status: *status,
            result: result.clone(),
            at: *at,
        },
        AgentEvent::RunEnd {
            run_id,
            status,
            outcome,
            at,
        } => SessionEvent::RunEnd {
            run_id: run_id.clone(),
            status: status.clone(),
            outcome: outcome.clone(),
            at: *at,
        },
    })
}
