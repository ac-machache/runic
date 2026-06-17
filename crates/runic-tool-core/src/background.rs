//! Long-running tools that don't block the agent loop.
//!
//! Third tool kind alongside `Tool` and `HitlTool`. The adapter spawns the
//! work in a tokio task, registers it with the `BackgroundManager`, and
//! returns a task id immediately. The model can keep working and check on
//! it later via the `background_status` tool, or call it off with
//! `background_cancel`.
//!
//! Live state (handles + results) lives in `BackgroundManager` which is
//! stashed in `RuntimeContext`. It is intentionally NOT serialized into
//! `AgentState.events` — process restart loses the manager (correct, the
//! work itself is gone). The audit trail comes for free from the normal
//! Message events: every `background_status` / `background_cancel`
//! invocation lands as a regular tool_use / tool_result pair in the event log.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use runic_message_types::ToolCall;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::tool::{Tool, ToolContext, ToolDispatch, ToolResult};

// ─── Trait & adapter ────────────────────────────────────────────────────────

/// A long-running tool. The adapter spawns `run` in a tokio task; the call
/// returns a task id immediately and the agent polls via `background_status`.
#[async_trait]
pub trait BackgroundTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    /// The actual long-running work. Same signature as `Tool::execute` —
    /// author writes this without thinking about spawning or task ids.
    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult;
}

pub struct BackgroundAdapter<T: BackgroundTool>(pub Arc<T>);

#[async_trait]
impl<T: BackgroundTool + 'static> ToolDispatch for BackgroundAdapter<T> {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn input_schema(&self) -> serde_json::Value {
        self.0.input_schema()
    }
    fn is_background(&self) -> bool {
        true
    }
    async fn dispatch(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult {
        let Some(manager) = ctx.get::<BackgroundManager>() else {
            return ToolResult::error(format!(
                "{}: BackgroundManager not in runtime context",
                call.name
            ));
        };

        let tool = self.0.clone();
        let input = call.input.clone();
        let ctx_clone = ctx.clone();
        let tool_name = self.0.name().to_string();

        let task_id = manager.start(tool_name.clone(), async move {
            tool.run(input, &ctx_clone).await
        });

        ToolResult::ok(format!(
            "Started '{tool_name}' in the background (id={task_id}) — it runs \
             WITHOUT blocking you. Do other useful work now: reply to the user, \
             call other tools, or start other tasks. Fetch the result with \
             `background_status` using this id when you need it; if it's still \
             running, move on and check back later rather than polling in a loop. \
             `background_cancel` aborts it."
        ))
    }
}

// ─── Manager ────────────────────────────────────────────────────────────────

/// Tracks live and finished background tasks. Lives in `RuntimeContext`.
#[derive(Default)]
pub struct BackgroundManager {
    tasks: Mutex<HashMap<String, TaskEntry>>,
}

struct TaskEntry {
    tool_name: String,
    started_at: DateTime<Utc>,
    /// Set exactly once — either by the spawned task on completion or by
    /// `abort()` on cancellation. `OnceLock::set` is atomic, so first
    /// writer wins. No race between "task finished naturally" vs "user
    /// cancelled at the same moment."
    finished: Arc<OnceLock<TaskFinishState>>,
    /// Kept so we can call `.abort()` from `BackgroundManager::abort`.
    handle: JoinHandle<()>,
}

enum TaskFinishState {
    Done {
        result: ToolResult,
        ended_at: DateTime<Utc>,
    },
    Cancelled {
        ended_at: DateTime<Utc>,
    },
}

/// Snapshot of a task's current state — what `BackgroundStatusTool` reads.
#[derive(Debug, Clone)]
pub enum TaskStatusView {
    Running {
        tool_name: String,
        started_at: DateTime<Utc>,
        elapsed_ms: u64,
    },
    Done {
        tool_name: String,
        result: ToolResult,
        duration_ms: u64,
    },
    Cancelled {
        tool_name: String,
        duration_ms: u64,
    },
}

/// Result of an `abort()` attempt — three distinct outcomes the tool can
/// surface to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortOutcome {
    /// We were the first writer; the task is now marked Cancelled.
    Cancelled,
    /// The task already finished (Done or previously Cancelled). No action taken.
    AlreadyFinished,
    /// No task with this id was ever started.
    NotFound,
}

impl BackgroundManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn `future` as a background task. Returns the task id.
    pub fn start<F>(&self, tool_name: String, future: F) -> String
    where
        F: std::future::Future<Output = ToolResult> + Send + 'static,
    {
        let task_id = Uuid::new_v4().to_string();
        let finished: Arc<OnceLock<TaskFinishState>> = Arc::new(OnceLock::new());
        let finished_for_task = finished.clone();

        let handle = tokio::spawn(async move {
            let result = future.await;
            // .set returns Err if abort beat us to it — that's fine, drop the result.
            let _ = finished_for_task.set(TaskFinishState::Done {
                result,
                ended_at: Utc::now(),
            });
        });

        let entry = TaskEntry {
            tool_name,
            started_at: Utc::now(),
            finished,
            handle,
        };
        self.tasks
            .lock()
            .expect("tasks lock poisoned")
            .insert(task_id.clone(), entry);
        task_id
    }

    /// Non-blocking peek at a task's state.
    pub fn check(&self, task_id: &str) -> Option<TaskStatusView> {
        let tasks = self.tasks.lock().expect("tasks lock poisoned");
        let entry = tasks.get(task_id)?;
        Some(match entry.finished.get() {
            None => {
                let elapsed_ms = (Utc::now() - entry.started_at)
                    .num_milliseconds()
                    .max(0) as u64;
                TaskStatusView::Running {
                    tool_name: entry.tool_name.clone(),
                    started_at: entry.started_at,
                    elapsed_ms,
                }
            }
            Some(TaskFinishState::Done { result, ended_at }) => {
                let duration_ms = (*ended_at - entry.started_at)
                    .num_milliseconds()
                    .max(0) as u64;
                TaskStatusView::Done {
                    tool_name: entry.tool_name.clone(),
                    result: result.clone(),
                    duration_ms,
                }
            }
            Some(TaskFinishState::Cancelled { ended_at }) => {
                let duration_ms = (*ended_at - entry.started_at)
                    .num_milliseconds()
                    .max(0) as u64;
                TaskStatusView::Cancelled {
                    tool_name: entry.tool_name.clone(),
                    duration_ms,
                }
            }
        })
    }

    /// Abort a running task. Returns whether the cancellation took effect.
    ///
    /// Race-safe: uses `OnceLock::set` to claim the finished slot. If the
    /// task naturally completed at the same moment, the natural completion
    /// wins and we return `AlreadyFinished`.
    pub fn abort(&self, task_id: &str) -> AbortOutcome {
        let tasks = self.tasks.lock().expect("tasks lock poisoned");
        let Some(entry) = tasks.get(task_id) else {
            return AbortOutcome::NotFound;
        };

        // Try to claim the finished slot with Cancelled.
        let claim = entry.finished.set(TaskFinishState::Cancelled {
            ended_at: Utc::now(),
        });

        match claim {
            Ok(()) => {
                // We won the race — kick the actual tokio task so it stops
                // making progress at its next .await point.
                entry.handle.abort();
                AbortOutcome::Cancelled
            }
            Err(_) => AbortOutcome::AlreadyFinished,
        }
    }

    /// Number of tracked tasks (running + finished + cancelled).
    pub fn len(&self) -> usize {
        self.tasks.lock().expect("tasks lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// All known task ids, sorted. Used by reminder sources that want to
    /// scan for newly-completed tasks each turn.
    pub fn task_ids(&self) -> Vec<String> {
        let tasks = self.tasks.lock().expect("tasks lock poisoned");
        let mut ids: Vec<String> = tasks.keys().cloned().collect();
        ids.sort();
        ids
    }
}

impl std::fmt::Debug for BackgroundManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundManager")
            .field("tasks", &self.len())
            .finish()
    }
}

// ─── Generic status-checking tool ───────────────────────────────────────────

/// Plain Tool the model calls to poll any background task by id.
/// Auto-registered by `AgentBuilder::background_tool` on first use.
pub struct BackgroundStatusTool;

#[async_trait]
impl Tool for BackgroundStatusTool {
    fn name(&self) -> &str {
        "background_status"
    }
    fn description(&self) -> &str {
        "Fetch a background task's result by id — returns running / done / cancelled \
         (the result comes with 'done'). If it's still running, don't loop on it: do \
         other work and check back later."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task id returned when the background tool was first called."
                }
            },
            "required": ["task_id"],
            "additionalProperties": false,
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let Some(manager) = ctx.get::<BackgroundManager>() else {
            return ToolResult::error("BackgroundManager not in runtime context");
        };
        let Some(task_id) = input.get("task_id").and_then(|v| v.as_str()) else {
            return ToolResult::error("missing required field 'task_id'");
        };
        match manager.check(task_id) {
            None => ToolResult::error(format!("task {task_id}: not found")),
            Some(TaskStatusView::Running {
                tool_name,
                elapsed_ms,
                ..
            }) => ToolResult::ok(format!(
                "task {task_id} ({tool_name}): running, elapsed={elapsed_ms}ms"
            )),
            Some(TaskStatusView::Done {
                tool_name,
                result,
                duration_ms,
            }) => {
                let tag = if result.is_error { "errored" } else { "done" };
                ToolResult::ok(format!(
                    "task {task_id} ({tool_name}): {tag} in {duration_ms}ms\n\
                     ─── result ───\n{}",
                    result.content
                ))
            }
            Some(TaskStatusView::Cancelled {
                tool_name,
                duration_ms,
            }) => ToolResult::ok(format!(
                "task {task_id} ({tool_name}): cancelled after {duration_ms}ms"
            )),
        }
    }
}

// ─── Generic cancellation tool ──────────────────────────────────────────────

/// Plain Tool the model calls to abort a running background task.
/// Auto-registered alongside `BackgroundStatusTool`.
pub struct BackgroundCancelTool;

#[async_trait]
impl Tool for BackgroundCancelTool {
    fn name(&self) -> &str {
        "background_cancel"
    }
    fn description(&self) -> &str {
        "Cancel a running background task. Has no effect if the task already finished or doesn't exist."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The task id to cancel."
                }
            },
            "required": ["task_id"],
            "additionalProperties": false,
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let Some(manager) = ctx.get::<BackgroundManager>() else {
            return ToolResult::error("BackgroundManager not in runtime context");
        };
        let Some(task_id) = input.get("task_id").and_then(|v| v.as_str()) else {
            return ToolResult::error("missing required field 'task_id'");
        };
        match manager.abort(task_id) {
            AbortOutcome::Cancelled => {
                ToolResult::ok(format!("task {task_id}: cancelled"))
            }
            AbortOutcome::AlreadyFinished => ToolResult::ok(format!(
                "task {task_id}: already finished, no action needed"
            )),
            AbortOutcome::NotFound => {
                ToolResult::error(format!("task {task_id}: not found"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn manager_start_returns_unique_ids() {
        let mgr = BackgroundManager::new();
        let id1 = mgr.start("a".into(), async { ToolResult::ok("done") });
        let id2 = mgr.start("a".into(), async { ToolResult::ok("done") });
        assert_ne!(id1, id2);
        assert_eq!(mgr.len(), 2);
    }

    #[tokio::test]
    async fn quickly_finished_task_reports_done() {
        let mgr = BackgroundManager::new();
        let id = mgr.start("instant".into(), async { ToolResult::ok("hello") });
        tokio::time::sleep(Duration::from_millis(20)).await;
        match mgr.check(&id) {
            Some(TaskStatusView::Done { result, .. }) => {
                assert_eq!(result.content, "hello");
                assert!(!result.is_error);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn slow_task_reports_running_then_done() {
        let mgr = BackgroundManager::new();
        let id = mgr.start("slow".into(), async {
            tokio::time::sleep(Duration::from_millis(80)).await;
            ToolResult::ok("finally")
        });
        match mgr.check(&id) {
            Some(TaskStatusView::Running { .. }) => {}
            other => panic!("expected Running, got {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        match mgr.check(&id) {
            Some(TaskStatusView::Done { result, .. }) => {
                assert_eq!(result.content, "finally");
            }
            other => panic!("expected Done after sleep, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn check_unknown_id_is_none() {
        let mgr = BackgroundManager::new();
        assert!(mgr.check("does-not-exist").is_none());
    }

    #[tokio::test]
    async fn abort_running_task_is_cancelled() {
        let mgr = BackgroundManager::new();
        let id = mgr.start("long".into(), async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            ToolResult::ok("never reached")
        });

        // Give the task a moment to actually start
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert_eq!(mgr.abort(&id), AbortOutcome::Cancelled);

        match mgr.check(&id) {
            Some(TaskStatusView::Cancelled { tool_name, .. }) => {
                assert_eq!(tool_name, "long");
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn abort_unknown_id_is_not_found() {
        let mgr = BackgroundManager::new();
        assert_eq!(mgr.abort("nope"), AbortOutcome::NotFound);
    }

    #[tokio::test]
    async fn abort_already_finished_task_is_already_finished() {
        let mgr = BackgroundManager::new();
        let id = mgr.start("quick".into(), async { ToolResult::ok("done") });

        // Wait for natural completion
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(mgr.abort(&id), AbortOutcome::AlreadyFinished);

        // State should still be Done, not Cancelled
        match mgr.check(&id) {
            Some(TaskStatusView::Done { .. }) => {}
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn double_abort_first_wins() {
        let mgr = BackgroundManager::new();
        let id = mgr.start("long".into(), async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            ToolResult::ok("never")
        });
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert_eq!(mgr.abort(&id), AbortOutcome::Cancelled);
        // Second abort: task is already marked Cancelled, so AlreadyFinished
        assert_eq!(mgr.abort(&id), AbortOutcome::AlreadyFinished);
    }
}
