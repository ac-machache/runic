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

use runic_agent::{Agent, AgentBuilder, CancelToken, RunContext, TasksSnapshot};
use runic_provider::Provider;
use runic_skills::SkillSet;
use runic_state::SessionEvent;
use runic_state::{ChildPersistenceHandle, ChildPersistenceStatus, ChildSink, ExternalEvents};
use runic_tool::{Tool, ToolContext, ToolResult};

use crate::subagent::Subagent;

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
    pub tenant: String,
    pub session: String,
    pub child_session: Option<String>,
    pub persistence: Option<ChildPersistenceHandle>,
}

pub struct SubagentReq<'a> {
    pub subagent: &'a Subagent,
    pub dctx: &'a DelegationCtx,
}

#[async_trait]
pub trait SubagentBuilder: Send + Sync {
    async fn provider(&self, req: &SubagentReq<'_>) -> Arc<dyn Provider>;

    fn default_model(&self, req: &SubagentReq<'_>) -> String;

    async fn tool_pool(&self, _req: &SubagentReq<'_>) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }

    fn skill_catalog(&self, _req: &SubagentReq<'_>) -> Option<Arc<SkillSet>> {
        None
    }

    fn identity(&self, req: &SubagentReq<'_>) -> (String, String) {
        (
            req.dctx.tenant.clone(),
            req.dctx
                .child_session
                .clone()
                .unwrap_or_else(|| format!("{}:{}", req.dctx.session, req.subagent.name)),
        )
    }

    fn decorate(&self, b: AgentBuilder, _req: &SubagentReq<'_>) -> AgentBuilder {
        b
    }
}

pub async fn assemble_subagent(builder: &dyn SubagentBuilder, req: &SubagentReq<'_>) -> Agent {
    let (tenant, session) = builder.identity(req);
    let provider = builder.provider(req).await;
    let model = req
        .subagent
        .model
        .clone()
        .unwrap_or_else(|| builder.default_model(req));
    let pool = builder.tool_pool(req).await;

    let scoped = builder
        .skill_catalog(req)
        .filter(|_| !req.subagent.skills.is_empty())
        .map(|catalog| Arc::new(catalog.scope_glob(&req.subagent.skills)))
        .filter(|set| !set.is_empty());

    let mut prompt = req.subagent.system_prompt.clone();
    if let Some(set) = &scoped {
        prompt = format!("{prompt}\n\n{}", set.prompt_section());
    }

    let mut b = Agent::builder(provider, tenant, session)
        .model(model)
        .system_prompt(prompt);
    for t in req.subagent.scope_tools(&pool) {
        b = b.tool(t);
    }
    if let Some(set) = &scoped
        && let Some(tool) = set.view_tool()
    {
        b = b.tool(tool);
    }
    if let Some(max_turns) = req.subagent.max_turns {
        b = b.max_turns(max_turns);
    }
    builder.decorate(b, req).build()
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

pub(crate) const DEFAULT_TOOL_NAME: &str = "delegate";
const DEFAULT_TOOL_DESCRIPTION: &str = "Delegate a self-contained task to a subagent (it does NOT see this \
     conversation). Pick `agent` from the roster. Use `parallel` to run \
     several at once, or `background` for long tasks (poll with \
     check_result).";

struct StaticBuilder {
    provider: Arc<dyn Provider>,
    model: String,
}

#[async_trait]
impl SubagentBuilder for StaticBuilder {
    async fn provider(&self, _req: &SubagentReq<'_>) -> Arc<dyn Provider> {
        self.provider.clone()
    }

    fn default_model(&self, _req: &SubagentReq<'_>) -> String {
        self.model.clone()
    }
}

pub struct DelegateTool {
    subagents: Vec<Subagent>,
    builder: Arc<dyn SubagentBuilder>,
    depth: u32,
    max_depth: u32,
    budget: Arc<SpawnBudget>,
    cancel: CancelToken,
    tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
    tag: Option<String>,
    intro: Option<String>,
    tool_name: Option<String>,
    tool_description: Option<String>,
}

impl DelegateTool {
    pub fn new(
        subagents: impl IntoIterator<Item = Subagent>,
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
    ) -> Self {
        Self::with_builder(
            subagents,
            Arc::new(StaticBuilder {
                provider,
                model: model.into(),
            }),
        )
    }

    pub fn with_builder(
        subagents: impl IntoIterator<Item = Subagent>,
        builder: Arc<dyn SubagentBuilder>,
    ) -> Self {
        Self {
            subagents: subagents.into_iter().collect(),
            builder,
            depth: 0,
            max_depth: DEFAULT_MAX_DEPTH,
            budget: SpawnBudget::new(DEFAULT_MAX_TOTAL_SPAWNS, DEFAULT_MAX_CONCURRENT),
            cancel: CancelToken::new(),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            tag: None,
            intro: None,
            tool_name: None,
            tool_description: None,
        }
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    pub fn intro(mut self, text: impl Into<String>) -> Self {
        self.intro = Some(text.into());
        self
    }

    pub fn tool_name(mut self, name: impl Into<String>) -> Self {
        self.tool_name = Some(name.into());
        self
    }

    pub fn tool_description(mut self, text: impl Into<String>) -> Self {
        self.tool_description = Some(text.into());
        self
    }

    pub fn roster_section(&self) -> String {
        let tag = self.tag.as_deref().unwrap_or(crate::subagent::DEFAULT_TAG);
        let intro = match &self.intro {
            Some(text) => text.clone(),
            None => crate::subagent::default_intro(self.resolved_tool_name()),
        };
        crate::subagent::render_roster(tag, &intro, &self.subagents)
    }

    fn resolved_tool_name(&self) -> &str {
        self.tool_name.as_deref().unwrap_or(DEFAULT_TOOL_NAME)
    }

    fn find(&self, name: &str) -> Option<&Subagent> {
        self.subagents.iter().find(|s| s.name == name)
    }

    fn roster_lines(&self) -> String {
        self.subagents
            .iter()
            .map(Subagent::roster_line)
            .collect::<Vec<_>>()
            .join("\n")
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
            tenant: ctx.user_id.clone(),
            session: ctx.session_id.clone(),
            child_session: None,
            persistence: ctx.get::<ChildPersistenceHandle>().map(|h| (*h).clone()),
        }
    }

    async fn delegate_one(&self, agent: &str, prompt: String, ctx: &ToolContext) -> ToolResult {
        let Some(sub) = self.find(agent).cloned() else {
            return ToolResult::error(format!(
                "unknown subagent '{agent}'. Available:\n{}",
                self.roster_lines()
            ));
        };
        let guard = match self.budget.acquire() {
            Ok(g) => g,
            Err(e) => return ToolResult::error(e),
        };
        let external = ctx.get::<ExternalEvents>();
        let (call_id, turn) = edge_keys(ctx);
        let mut dctx = self.child_ctx(self.cancel.clone(), ctx);
        let sink = begin_child(&mut dctx, agent).await;
        emit_started(
            &external,
            &ctx.run_id,
            turn,
            &call_id,
            agent,
            runic_state::DelegationMode::Sync,
            dctx.child_session.clone(),
        );
        let started = std::time::Instant::now();
        let outcome = run_child(&self.builder, &sub, &dctx, &prompt, sink).await;
        emit_finished(
            &external,
            &ctx.run_id,
            turn,
            &call_id,
            agent,
            &outcome,
            started,
        );
        drop(guard);
        match outcome.result {
            Ok(child) => ToolResult::ok(child.text),
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
            let sub = self.find(&name).cloned();
            let builder = self.builder.clone();
            let acquired = self.budget.acquire();
            let mut dctx = self.child_ctx(self.cancel.clone(), ctx);
            let external = ctx.get::<ExternalEvents>();
            let (call_id, turn) = edge_keys(ctx);
            let run_id = ctx.run_id.clone();
            async move {
                let Some(sub) = sub else {
                    return format!("[{name}] error: unknown subagent");
                };
                let guard = match acquired {
                    Ok(g) => g,
                    Err(e) => return format!("[{name}] error: {e}"),
                };
                let sink = begin_child(&mut dctx, &name).await;
                emit_started(
                    &external,
                    &run_id,
                    turn,
                    &call_id,
                    &name,
                    runic_state::DelegationMode::Parallel,
                    dctx.child_session.clone(),
                );
                let started = std::time::Instant::now();
                let outcome = run_child(&builder, &sub, &dctx, &prompt, sink).await;
                emit_finished(&external, &run_id, turn, &call_id, &name, &outcome, started);
                let out = match outcome.result {
                    Ok(child) => format!("[{name}]\n{}", child.text),
                    Err(e) => format!("[{name}] error: {e}"),
                };
                drop(guard);
                out
            }
        });
        let outputs = futures::future::join_all(futures).await;
        ToolResult::ok(outputs.join("\n\n---\n\n"))
    }

    async fn delegate_background(
        &self,
        agent: &str,
        prompt: String,
        ctx: &ToolContext,
    ) -> ToolResult {
        let Some(sub) = self.find(agent).cloned() else {
            return ToolResult::error(format!("unknown subagent '{agent}'"));
        };
        let guard = match self.budget.acquire() {
            Ok(g) => g,
            Err(e) => return ToolResult::error(e),
        };

        let task_id = format!("task-{}", uuid::Uuid::new_v4().simple());
        let cancel = CancelToken::new();
        self.tasks.lock().unwrap_or_else(|p| p.into_inner()).insert(
            task_id.clone(),
            BackgroundTask {
                agent: agent.to_string(),
                status: TaskStatus::Running,
                output: None,
                error: None,
                cancel: cancel.clone(),
            },
        );

        let external = ctx.get::<ExternalEvents>();
        let run_id = ctx.run_id.clone();
        let mut dctx = self.child_ctx(cancel, ctx);
        let sink = begin_child(&mut dctx, agent).await;
        if let Some(external) = &external {
            external.emit(SessionEvent::TaskSpawned {
                run_id: run_id.clone(),
                task_id: task_id.clone(),
                agent: agent.to_string(),
                prompt: head(&prompt, 300),
                child_session: dctx.child_session.clone(),
                at: chrono::Utc::now(),
            });
        }

        let builder = self.builder.clone();
        let tasks = self.tasks.clone();
        let tid = task_id.clone();
        let (call_id, turn) = edge_keys(ctx);
        let agent_name = agent.to_string();
        tokio::spawn(async move {
            let _guard = guard; // hold the concurrent slot until done
            emit_started(
                &external,
                &run_id,
                turn,
                &call_id,
                &agent_name,
                runic_state::DelegationMode::Background,
                dctx.child_session.clone(),
            );
            let started = std::time::Instant::now();
            let outcome = run_child(&builder, &sub, &dctx, &prompt, sink).await;
            emit_finished(
                &external,
                &run_id,
                turn,
                &call_id,
                &agent_name,
                &outcome,
                started,
            );
            let result = outcome.result;
            let outcome = {
                let mut tasks = tasks.lock().unwrap_or_else(|p| p.into_inner());
                let Some(task) = tasks.get_mut(&tid) else {
                    return;
                };
                if task.status == TaskStatus::Cancelled {
                    return;
                }
                match result {
                    Ok(child) => {
                        task.status = TaskStatus::Completed;
                        task.output = Some(child.text.clone());
                        (runic_state::TaskStatus::Completed, Some(child.text))
                    }
                    Err(e) => {
                        task.status = TaskStatus::Failed;
                        task.error = Some(e.to_string());
                        (runic_state::TaskStatus::Failed, Some(e.to_string()))
                    }
                }
            };
            if let Some(external) = &external {
                external.emit(SessionEvent::TaskFinished {
                    run_id,
                    task_id: tid,
                    status: outcome.0,
                    result: outcome.1,
                    at: chrono::Utc::now(),
                });
            }
        });

        ToolResult::ok(format!(
            "Delegated to '{agent}' in the background. Poll with action=check_result, task_id={task_id}"
        ))
    }

    fn check_result(&self, task_id: &str, ctx: &ToolContext) -> ToolResult {
        if let Some(task) = self
            .tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(task_id)
        {
            return match task.status {
                TaskStatus::Running => ToolResult::ok(format!("task '{task_id}' is still running")),
                TaskStatus::Completed => ToolResult::ok(task.output.clone().unwrap_or_default()),
                TaskStatus::Failed => ToolResult::error(format!(
                    "task '{task_id}' failed: {}",
                    task.error.clone().unwrap_or_default()
                )),
                TaskStatus::Cancelled => ToolResult::ok(format!("task '{task_id}' was cancelled")),
            };
        }
        let Some(snapshot) = ctx.get::<TasksSnapshot>() else {
            return ToolResult::error(format!("no such task '{task_id}'"));
        };
        match snapshot.0.get(task_id) {
            None => ToolResult::error(format!("no such task '{task_id}'")),
            Some(record) => match record.status {
                runic_state::TaskStatus::Running => ToolResult::ok(format!(
                    "task '{task_id}' was orphaned by a restart — its live handle is gone"
                )),
                runic_state::TaskStatus::Completed => {
                    ToolResult::ok(record.result.clone().unwrap_or_default())
                }
                runic_state::TaskStatus::Failed => ToolResult::error(format!(
                    "task '{task_id}' failed: {}",
                    record.result.clone().unwrap_or_default()
                )),
                runic_state::TaskStatus::Cancelled => {
                    ToolResult::ok(format!("task '{task_id}' was cancelled"))
                }
            },
        }
    }

    fn list_results(&self, ctx: &ToolContext) -> ToolResult {
        let mut lines: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        if let Some(snapshot) = ctx.get::<TasksSnapshot>() {
            for (id, record) in snapshot.0.iter() {
                lines.insert(
                    id.clone(),
                    format!("- {id}: {} [{:?}]", record.agent, record.status),
                );
            }
        }
        for (id, task) in self.tasks.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            lines.insert(
                id.clone(),
                format!("- {id}: {} [{:?}]", task.agent, task.status),
            );
        }
        if lines.is_empty() {
            return ToolResult::ok("no background delegations");
        }
        ToolResult::ok(lines.into_values().collect::<Vec<_>>().join("\n"))
    }

    fn cancel_task(&self, task_id: &str, ctx: &ToolContext) -> ToolResult {
        let transitioned = {
            match self
                .tasks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get_mut(task_id)
            {
                None => return ToolResult::error(format!("no such task '{task_id}'")),
                Some(task) => {
                    if task.status == TaskStatus::Running {
                        task.cancel.cancel();
                        task.status = TaskStatus::Cancelled;
                        true
                    } else {
                        return ToolResult::ok(format!(
                            "task '{task_id}' already {:?}",
                            task.status
                        ));
                    }
                }
            }
        };
        if transitioned && let Some(external) = ctx.get::<ExternalEvents>() {
            external.emit(SessionEvent::TaskFinished {
                run_id: ctx.run_id.clone(),
                task_id: task_id.to_string(),
                status: runic_state::TaskStatus::Cancelled,
                result: None,
                at: chrono::Utc::now(),
            });
        }
        ToolResult::ok(format!("cancelled task '{task_id}'"))
    }
}

fn head(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let h: String = s.chars().take(max).collect();
    format!("{h}…")
}

/// Build + run a child agent to completion; return its final assistant text.
struct ChildRun {
    text: String,
    usage: runic_types::TokenUsage,
    model: String,
}

struct ChildOutcome {
    result: anyhow::Result<ChildRun>,
    child_session: Option<String>,
    persistence: Option<ChildPersistenceStatus>,
}

async fn begin_child(dctx: &mut DelegationCtx, agent: &str) -> Option<Box<dyn ChildSink>> {
    let handle = dctx.persistence.clone()?;
    match handle.0.begin(agent).await {
        Ok(sink) => {
            dctx.child_session = Some(sink.session_id().to_string());
            Some(sink)
        }
        Err(e) => {
            tracing::warn!(agent, error = %e, "child persistence begin failed; running ephemeral");
            None
        }
    }
}

async fn run_child(
    builder: &Arc<dyn SubagentBuilder>,
    subagent: &Subagent,
    dctx: &DelegationCtx,
    prompt: &str,
    sink: Option<Box<dyn ChildSink>>,
) -> ChildOutcome {
    let req = SubagentReq { subagent, dctx };
    let mut child = assemble_subagent(builder.as_ref(), &req).await;
    let configured = child.model().to_string();
    if let Some(sink) = &sink {
        child.state_mut().set_persist_tx(sink.sink());
    }
    let mut rc = RunContext::new()
        .with_cancel(dctx.cancel.clone())
        .with_config(dctx.config.clone());
    if let Some(sink) = &sink {
        rc = rc.with_child_persistence(sink.nested());
    }
    let result = match child.run_with(prompt.to_string(), rc).await {
        Ok(outcome) => {
            let served = child
                .state()
                .events()
                .iter()
                .rev()
                .find_map(|event| match event {
                    SessionEvent::TurnEnd { model, .. } => Some(model.clone()),
                    _ => None,
                })
                .unwrap_or(configured);
            Ok(ChildRun {
                text: child.state().last_assistant_text().unwrap_or_default(),
                usage: outcome.usage,
                model: served,
            })
        }
        Err(e) => Err(anyhow::anyhow!("{e}")),
    };
    let persistence = match &sink {
        Some(sink) => Some(match sink.flush().await {
            Ok(()) => ChildPersistenceStatus::Flushed,
            Err(e) => ChildPersistenceStatus::FlushFailed(e.to_string()),
        }),
        None => None,
    };
    ChildOutcome {
        result,
        child_session: dctx.child_session.clone(),
        persistence,
    }
}

fn edge_keys(ctx: &ToolContext) -> (String, u32) {
    (
        ctx.get::<runic_tool::CallId>()
            .map(|c| c.0.clone())
            .unwrap_or_default(),
        ctx.get::<runic_tool::CurrentTurn>()
            .map(|t| t.0)
            .unwrap_or_default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn emit_finished(
    external: &Option<std::sync::Arc<ExternalEvents>>,
    run_id: &str,
    turn: u32,
    call_id: &str,
    agent: &str,
    outcome: &ChildOutcome,
    started: std::time::Instant,
) {
    let Some(external) = external else { return };
    let (status, usage, model) = match &outcome.result {
        Ok(child) => (
            runic_state::DelegationStatus::Ok,
            child.usage,
            Some(child.model.clone()),
        ),
        Err(e) => (
            runic_state::DelegationStatus::Failed(e.to_string()),
            runic_types::TokenUsage::default(),
            None,
        ),
    };
    external.emit(SessionEvent::DelegationFinished {
        run_id: run_id.to_string(),
        turn,
        call_id: call_id.to_string(),
        agent: agent.to_string(),
        status,
        usage,
        model,
        duration_ms: started.elapsed().as_millis() as u64,
        child_session: outcome.child_session.clone(),
        child_persistence: outcome.persistence.clone(),
        at: chrono::Utc::now(),
    });
}

#[allow(clippy::too_many_arguments)]
fn emit_started(
    external: &Option<std::sync::Arc<ExternalEvents>>,
    run_id: &str,
    turn: u32,
    call_id: &str,
    agent: &str,
    mode: runic_state::DelegationMode,
    child_session: Option<String>,
) {
    let Some(external) = external else { return };
    external.emit(SessionEvent::DelegationStarted {
        run_id: run_id.to_string(),
        turn,
        call_id: call_id.to_string(),
        agent: agent.to_string(),
        mode,
        child_session,
        at: chrono::Utc::now(),
    });
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
        self.resolved_tool_name()
    }

    fn description(&self) -> &str {
        self.tool_description
            .as_deref()
            .unwrap_or(DEFAULT_TOOL_DESCRIPTION)
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
                Some(id) => self.check_result(id, ctx),
                None => ToolResult::error("check_result requires task_id"),
            },
            "list_results" => self.list_results(ctx),
            "cancel_task" => match args.get("task_id").and_then(|v| v.as_str()) {
                Some(id) => self.cancel_task(id, ctx),
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
                    self.delegate_background(agent, full, ctx).await
                } else {
                    self.delegate_one(agent, full, ctx).await
                }
            }
            other => ToolResult::error(format!("unknown action '{other}'")),
        };
        Ok(result)
    }
}
