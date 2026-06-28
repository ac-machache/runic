//! The single `delegate` tool — ZeroClaw's delegation shape on runic's
//! primitives. The model calls one tool, picks a subagent from the roster by
//! name, and the parent gets the child's final answer back.
//!
//! Safeguards (all from ZeroClaw):
//! - **depth limit** — a child at `max_depth` can't delegate further;
//! - **no-escalation** — the [`SubagentBuilder`] scopes the child's tools to a
//!   subset of the parent's (rejecting unknown names);
//! - **spawn budget** — caps total + concurrent child runs per parent;
//! - **cancellation cascade** — children carry a [`CancelToken`]; a background
//!   task gets its own, cancellable via `cancel_task`.
//!
//! Actions: `delegate` (sync, or `parallel`, or `background`), `check_result`,
//! `list_results`, `cancel_task`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use runic_agent::{Agent, CancelToken, RunContext};
use runic_tool::{Tool, ToolContext, ToolResult};

use crate::def::{AgentDef, AgentRoster};

/// Default maximum delegation depth (parent=0, so this allows 3 levels).
pub const DEFAULT_MAX_DEPTH: u32 = 3;
/// Default lifetime cap on child spawns per parent run.
pub const DEFAULT_MAX_TOTAL_SPAWNS: u32 = 16;
/// Default cap on concurrently-running children.
pub const DEFAULT_MAX_CONCURRENT: u32 = 4;

/// Context handed to a [`SubagentBuilder`] — the child's depth (already
/// incremented), the depth ceiling, the cancel token, and the parent run's
/// open config map, propagated to the child so per-run values (tenant ids,
/// etc.) reach the child's tools/hooks. Open by design: add whatever the app
/// needs onto `config` (it already carries `user_id`/`org_id` when set).
#[derive(Clone)]
pub struct DelegationCtx {
    pub depth: u32,
    pub max_depth: u32,
    pub cancel: CancelToken,
    /// The parent run's open per-run config map, carried to the child.
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// Builds the child [`Agent`] for a subagent definition. The app implements
/// this — it owns provider resolution and tool scoping. It MUST enforce the
/// no-escalation rule by routing the parent's tool pool through
/// [`AgentDef::scope_tools`], which keeps only the tools the def's
/// `allowed_tools` permits. `runic-subagent` owns the orchestration
/// (depth/budget/cancel).
#[async_trait]
pub trait SubagentBuilder: Send + Sync {
    async fn build(&self, def: &AgentDef, dctx: &DelegationCtx) -> anyhow::Result<Agent>;
}

/// Total + concurrent spawn budget, shared across a parent's delegate calls.
#[derive(Debug)]
pub struct SpawnBudget {
    max_total: u32,
    max_concurrent: u32,
    total: AtomicU32,
    concurrent: AtomicU32,
}

impl SpawnBudget {
    pub fn new(max_total: u32, max_concurrent: u32) -> Arc<Self> {
        Arc::new(Self {
            max_total,
            max_concurrent,
            total: AtomicU32::new(0),
            concurrent: AtomicU32::new(0),
        })
    }

    /// Reserve one spawn slot, or explain why not. The returned guard releases
    /// the concurrent slot on drop; the lifetime total is never released.
    fn acquire(self: &Arc<Self>) -> Result<BudgetGuard, String> {
        let total = self.total.fetch_add(1, Ordering::SeqCst) + 1;
        if total > self.max_total {
            self.total.fetch_sub(1, Ordering::SeqCst);
            return Err(format!(
                "spawn budget exhausted ({} total child runs this turn)",
                self.max_total
            ));
        }
        let concurrent = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        if concurrent > self.max_concurrent {
            self.concurrent.fetch_sub(1, Ordering::SeqCst);
            self.total.fetch_sub(1, Ordering::SeqCst);
            return Err(format!(
                "too many concurrent subagents (max {})",
                self.max_concurrent
            ));
        }
        Ok(BudgetGuard {
            budget: self.clone(),
        })
    }
}

struct BudgetGuard {
    budget: Arc<SpawnBudget>,
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        self.budget.concurrent.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Status of a background delegation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// A background delegation's tracked state.
#[derive(Clone)]
pub struct BackgroundTask {
    pub agent: String,
    pub status: TaskStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub cancel: CancelToken,
}

/// The `delegate` tool.
pub struct DelegateTool {
    roster: Arc<AgentRoster>,
    builder: Arc<dyn SubagentBuilder>,
    depth: u32,
    max_depth: u32,
    budget: Arc<SpawnBudget>,
    cancel: CancelToken,
    tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
}

impl DelegateTool {
    /// A root delegate tool (depth 0) with default safeguards.
    pub fn new(roster: Arc<AgentRoster>, builder: Arc<dyn SubagentBuilder>) -> Self {
        Self {
            roster,
            builder,
            depth: 0,
            max_depth: DEFAULT_MAX_DEPTH,
            budget: SpawnBudget::new(DEFAULT_MAX_TOTAL_SPAWNS, DEFAULT_MAX_CONCURRENT),
            cancel: CancelToken::new(),
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }
    pub fn with_max_depth(mut self, max_depth: u32) -> Self {
        self.max_depth = max_depth;
        self
    }
    pub fn with_budget(mut self, budget: Arc<SpawnBudget>) -> Self {
        self.budget = budget;
        self
    }
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Whether this tool may still delegate (depth ceiling not reached).
    fn can_delegate(&self) -> bool {
        self.depth < self.max_depth
    }

    fn child_ctx(&self, cancel: CancelToken, ctx: &ToolContext) -> DelegationCtx {
        DelegationCtx {
            depth: self.depth + 1,
            max_depth: self.max_depth,
            cancel,
            config: ctx.config_map().clone(),
        }
    }

    async fn delegate_one(&self, agent: &str, prompt: String, ctx: &ToolContext) -> ToolResult {
        let Some(def) = self.roster.get(agent).cloned() else {
            return ToolResult::error(format!(
                "unknown subagent '{agent}'. Available:\n{}",
                self.roster.roster_lines()
            ));
        };
        let guard = match self.budget.acquire() {
            Ok(g) => g,
            Err(e) => return ToolResult::error(e),
        };
        let dctx = self.child_ctx(self.cancel.clone(), ctx);
        let result = run_child(&self.builder, &def, &dctx, &prompt).await;
        drop(guard);
        match result {
            Ok(text) => ToolResult::ok(text),
            Err(e) => ToolResult::error(format!("subagent '{agent}' failed: {e}")),
        }
    }

    async fn delegate_parallel(
        &self,
        agents: &[String],
        prompt: String,
        ctx: &ToolContext,
    ) -> ToolResult {
        let futures = agents.iter().map(|name| {
            let name = name.clone();
            let prompt = prompt.clone();
            let def = self.roster.get(&name).cloned();
            let builder = self.builder.clone();
            let acquired = self.budget.acquire();
            let dctx = self.child_ctx(self.cancel.clone(), ctx);
            async move {
                let Some(def) = def else {
                    return format!("[{name}] error: unknown subagent");
                };
                let guard = match acquired {
                    Ok(g) => g,
                    Err(e) => return format!("[{name}] error: {e}"),
                };
                let out = match run_child(&builder, &def, &dctx, &prompt).await {
                    Ok(text) => format!("[{name}]\n{text}"),
                    Err(e) => format!("[{name}] error: {e}"),
                };
                drop(guard);
                out
            }
        });
        let outputs = futures::future::join_all(futures).await;
        ToolResult::ok(outputs.join("\n\n---\n\n"))
    }

    fn delegate_background(&self, agent: &str, prompt: String, ctx: &ToolContext) -> ToolResult {
        let Some(def) = self.roster.get(agent).cloned() else {
            return ToolResult::error(format!("unknown subagent '{agent}'"));
        };
        let guard = match self.budget.acquire() {
            Ok(g) => g,
            Err(e) => return ToolResult::error(e),
        };

        let task_id = format!("task-{}", uuid::Uuid::new_v4().simple());
        let cancel = CancelToken::new();
        self.tasks.lock().unwrap().insert(
            task_id.clone(),
            BackgroundTask {
                agent: agent.to_string(),
                status: TaskStatus::Running,
                output: None,
                error: None,
                cancel: cancel.clone(),
            },
        );

        let builder = self.builder.clone();
        let tasks = self.tasks.clone();
        let dctx = self.child_ctx(cancel, ctx);
        let tid = task_id.clone();
        tokio::spawn(async move {
            let _guard = guard; // hold the concurrent slot until done
            let result = run_child(&builder, &def, &dctx, &prompt).await;
            if let Some(task) = tasks.lock().unwrap().get_mut(&tid) {
                if task.status == TaskStatus::Cancelled {
                    return;
                }
                match result {
                    Ok(text) => {
                        task.status = TaskStatus::Completed;
                        task.output = Some(text);
                    }
                    Err(e) => {
                        task.status = TaskStatus::Failed;
                        task.error = Some(e.to_string());
                    }
                }
            }
        });

        ToolResult::ok(format!(
            "Delegated to '{agent}' in the background. Poll with action=check_result, task_id={task_id}"
        ))
    }

    fn check_result(&self, task_id: &str) -> ToolResult {
        match self.tasks.lock().unwrap().get(task_id) {
            None => ToolResult::error(format!("no such task '{task_id}'")),
            Some(task) => match task.status {
                TaskStatus::Running => ToolResult::ok(format!("task '{task_id}' is still running")),
                TaskStatus::Completed => ToolResult::ok(task.output.clone().unwrap_or_default()),
                TaskStatus::Failed => ToolResult::error(format!(
                    "task '{task_id}' failed: {}",
                    task.error.clone().unwrap_or_default()
                )),
                TaskStatus::Cancelled => ToolResult::ok(format!("task '{task_id}' was cancelled")),
            },
        }
    }

    fn list_results(&self) -> ToolResult {
        let tasks = self.tasks.lock().unwrap();
        if tasks.is_empty() {
            return ToolResult::ok("no background delegations");
        }
        let lines: Vec<String> = tasks
            .iter()
            .map(|(id, t)| format!("- {id}: {} [{:?}]", t.agent, t.status))
            .collect();
        ToolResult::ok(lines.join("\n"))
    }

    fn cancel_task(&self, task_id: &str) -> ToolResult {
        match self.tasks.lock().unwrap().get_mut(task_id) {
            None => ToolResult::error(format!("no such task '{task_id}'")),
            Some(task) => {
                if task.status == TaskStatus::Running {
                    task.cancel.cancel();
                    task.status = TaskStatus::Cancelled;
                    ToolResult::ok(format!("cancelled task '{task_id}'"))
                } else {
                    ToolResult::ok(format!("task '{task_id}' already {:?}", task.status))
                }
            }
        }
    }
}

/// Build + run a child agent to completion; return its final assistant text.
async fn run_child(
    builder: &Arc<dyn SubagentBuilder>,
    def: &AgentDef,
    dctx: &DelegationCtx,
    prompt: &str,
) -> anyhow::Result<String> {
    let mut child = builder.build(def, dctx).await?;
    let rc = RunContext::new()
        .with_cancel(dctx.cancel.clone())
        .with_config(dctx.config.clone());
    child
        .run_with(prompt.to_string(), rc)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(child.state().last_assistant_text().unwrap_or_default())
}

/// Combine optional context with the task prompt (ZeroClaw's framing).
fn compose_prompt(context: Option<&str>, prompt: &str) -> String {
    match context {
        Some(ctx) if !ctx.trim().is_empty() => format!("[Context]\n{ctx}\n\n[Task]\n{prompt}"),
        _ => prompt.to_string(),
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        // Static base; the roster is surfaced via the system prompt, like MCP
        // deferred tools (kept out of this &str which must be 'static-ish).
        "Delegate a self-contained task to a subagent (it does NOT see this \
         conversation). Pick `agent` from the roster. Use `parallel` to run \
         several at once, or `background` for long tasks (poll with \
         check_result)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["delegate", "check_result", "list_results", "cancel_task"],
                    "default": "delegate"
                },
                "agent": { "type": "string", "description": "Subagent name from the roster." },
                "prompt": { "type": "string", "description": "Self-contained task for the subagent." },
                "context": { "type": "string", "description": "Optional context prepended to the task." },
                "background": { "type": "boolean", "description": "Run detached; returns a task_id.", "default": false },
                "parallel": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Run these subagents concurrently with the same prompt."
                },
                "task_id": { "type": "string", "description": "For check_result / cancel_task." }
            }
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("delegate");

        let result = match action {
            "check_result" => match args.get("task_id").and_then(|v| v.as_str()) {
                Some(id) => self.check_result(id),
                None => ToolResult::error("check_result requires task_id"),
            },
            "list_results" => self.list_results(),
            "cancel_task" => match args.get("task_id").and_then(|v| v.as_str()) {
                Some(id) => self.cancel_task(id),
                None => ToolResult::error("cancel_task requires task_id"),
            },
            "delegate" => {
                if !self.can_delegate() {
                    return Ok(ToolResult::error(format!(
                        "delegation depth limit reached ({}/{}); cannot delegate further",
                        self.depth, self.max_depth
                    )));
                }
                let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
                if prompt.trim().is_empty() {
                    return Ok(ToolResult::error("delegate requires a non-empty prompt"));
                }
                let context = args.get("context").and_then(|v| v.as_str());
                let full = compose_prompt(context, prompt);

                // Parallel mode.
                if let Some(arr) = args.get("parallel").and_then(|v| v.as_array()) {
                    let agents: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect();
                    if !agents.is_empty() {
                        return Ok(self.delegate_parallel(&agents, full, ctx).await);
                    }
                }

                let Some(agent) = args.get("agent").and_then(|v| v.as_str()) else {
                    return Ok(ToolResult::error(
                        "delegate requires `agent` (or `parallel`)",
                    ));
                };
                let background = args
                    .get("background")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if background {
                    self.delegate_background(agent, full, ctx)
                } else {
                    self.delegate_one(agent, full, ctx).await
                }
            }
            other => ToolResult::error(format!("unknown action '{other}'")),
        };
        Ok(result)
    }
}
