//! `runic-agent` — Layer 5: the agent loop.
//!
//! The first thing that depends on *all* the L2 contracts ([`Provider`],
//! [`Tool`], the hook traits, [`AgentState`]). It wires them into a turn loop.
//!
//! **Design provenance** (best-of-three synthesis):
//! - **structure** ← ZeroClaw: the loop body is a thin orchestrator
//!   ([`turn::run_one_turn`]) that reads as a flat sequence of named step
//!   functions, each in its own module under `turn/`. No god-function.
//! - **dispatch / hooks / state** ← runic: the 3-phase tool dispatch
//!   ([`turn::dispatch`]), the two-trait hook fan-out, event-sourced state.
//! - **safety machinery** ← OpenFang: [`loop_guard`] (runaway detection),
//!   [`retry`] (backoff), per-tool timeouts — slotted in as discrete steps.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use runic_hook::{ReadHook, WriteHook};
use runic_provider::{CompletionRequest, Provider, ProviderError};
use runic_state::{AgentState, HookLifecycle};
use runic_tool::{
    ACTIVATED_KEY_PREFIX, ActivatedToolSet, HumanInterface, Tool, ToolCatalog, ToolSpec,
};
use tokio::sync::mpsc;

mod external;
pub(crate) mod run;
mod spill;
mod turn;

pub mod loop_guard;
pub mod retry;

pub use external::{ReminderQueue, TasksSnapshot};
pub use runic_state::RunOutcome;
pub use spill::{SPILL_PREVIEW_CHARS, SpilledArtifact, ToolOutputSpill};

/// Default hard cap on model turns per run — a backstop against runaway loops
/// (the tunable policy lives in [`loop_guard`] and hooks).
pub const DEFAULT_MAX_TURNS: u32 = 64;
/// Default per-tool execution timeout.
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;
/// Default max output tokens per model call.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

pub(crate) struct PendingDeferral {
    pub(crate) call_id: String,
    pub(crate) channel: String,
    pub(crate) payload: serde_json::Value,
}

/// A fallback `(provider, model)` the loop tries, in order, when the primary
/// model call fails with a not-found or persistent-transient error.
#[derive(Clone)]
pub struct FallbackProvider {
    /// The provider to call.
    pub provider: Arc<dyn Provider>,
    /// The model identifier to request from it.
    pub model: String,
}

/// Live events emitted during a run when a streaming sink is attached
/// (`RunContext::with_events`). Coarser, persistence-grade events still flow
/// through `AgentState`'s `SessionEvent` broadcast; this stream adds the
/// token-level + tool-lifecycle granularity a UI needs.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    RunStarted {
        run_id: String,
    },
    TextDelta(String),
    ThinkingDelta(String),
    ToolStarted {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
        result: serde_json::Value,
        provenance: Vec<runic_types::ProvenanceSource>,
    },
    TurnCompleted {
        turn: u32,
        stop_reason: String,
    },
    ToolDeferred {
        run_id: String,
        call_id: String,
        channel: String,
        payload: serde_json::Value,
    },
    RunCompleted(RunOutcome),
    /// Only fires for a non-`Continue` outcome.
    HookFired {
        hook_name: String,
        hook_kind: &'static str,
        lifecycle: HookLifecycle,
        outcome: &'static str,
        note: Option<String>,
    },
}

/// A cheap, cloneable cancellation flag. The loop checks it at each turn
/// boundary; flipping it (e.g. from a UI "stop" button) ends the run
/// gracefully after the current turn.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, un-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }
    /// Request cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    pub fn is_same(&self, other: &CancelToken) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Per-run context injected at invoke time (the langgraph/deepagents pattern:
/// build the agent once, vary the request data per run). A pooled agent reused
/// across runs carries request-varying data here rather than baking it in.
#[derive(Default)]
pub struct RunContext {
    /// Open per-run config map (user_id, org_id, flags, …). Overwrites
    /// `AgentState.config` for this run; never leaks across runs.
    pub config: serde_json::Map<String, serde_json::Value>,
    /// Optional per-run main-model override — swaps the agent's provider for
    /// this run only, then restores.
    pub provider: Option<Arc<dyn Provider>>,
    /// Optional cancellation token checked at each turn boundary.
    pub cancel: Option<CancelToken>,
    /// Optional steering channel: text pushed here is injected into the
    /// conversation as a user message at the start of the next turn.
    pub steering: Option<mpsc::UnboundedReceiver<String>>,
    /// Optional live-event sink. When set, the loop streams the model via
    /// `Provider::stream` and emits [`AgentEvent`]s (token deltas + tool
    /// lifecycle) here.
    pub events: Option<mpsc::UnboundedSender<AgentEvent>>,
    /// Optional human channel for HITL tools (`ask_user` / `escalate_to_human`).
    /// Provided per run by the surface; flows into [`ToolContext`].
    pub human: Option<Arc<dyn HumanInterface>>,
    /// Optional agent name recorded on the run's `RunStart` event.
    pub agent: Option<String>,
    /// Optional actor identifier recorded on the run's audit stamp. The
    /// application chooses what goes here (raw id, hash, or nothing) — the
    /// open `config` map is never persisted.
    pub actor: Option<String>,
    /// Optional pre-generated run id; the loop generates one when absent.
    pub run_id: Option<String>,
    /// Invocation mode recorded on the run trace span (`stream` / `wait` /
    /// `background` / `queued`); defaults to `direct`.
    pub mode: Option<&'static str>,
    /// Optional child-transcript persistence for this run's delegations. When
    /// set, the delegate tool begins a durable child session per delegation;
    /// absent → children stay ephemeral.
    pub child_persistence: Option<runic_state::ChildPersistenceHandle>,
}

impl RunContext {
    /// An empty context (equivalent to a bare `run`).
    pub fn new() -> Self {
        Self::default()
    }
    /// Set a single per-run config value.
    pub fn config_value(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.config.insert(key.into(), value);
        self
    }
    /// Replace the whole per-run config map.
    pub fn with_config(mut self, config: serde_json::Map<String, serde_json::Value>) -> Self {
        self.config = config;
        self
    }
    /// Override the model provider for this run.
    pub fn with_provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }
    /// Attach a cancellation token.
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = Some(cancel);
        self
    }
    /// Attach a steering receiver.
    pub fn with_steering(mut self, steering: mpsc::UnboundedReceiver<String>) -> Self {
        self.steering = Some(steering);
        self
    }
    /// Attach a live-event sink (enables streaming for this run).
    pub fn with_events(mut self, events: mpsc::UnboundedSender<AgentEvent>) -> Self {
        self.events = Some(events);
        self
    }
    /// Attach a human channel for HITL tools this run.
    pub fn with_human(mut self, human: Arc<dyn HumanInterface>) -> Self {
        self.human = Some(human);
        self
    }
    pub fn with_agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }

    pub fn with_child_persistence(mut self, handle: runic_state::ChildPersistenceHandle) -> Self {
        self.child_persistence = Some(handle);
        self
    }

    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    pub fn with_mode(mut self, mode: &'static str) -> Self {
        self.mode = Some(mode);
        self
    }
}

/// Tunable knobs for a run.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub max_turns: u32,
    pub tool_timeout: Duration,
    pub graceful_max_turns: bool,
    pub output_schema: Option<serde_json::Value>,
    pub thinking: Option<runic_provider::ThinkingConfig>,
    pub auto_spill_over: Option<usize>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: 1.0,
            max_turns: DEFAULT_MAX_TURNS,
            tool_timeout: Duration::from_secs(DEFAULT_TOOL_TIMEOUT_SECS),
            graceful_max_turns: false,
            output_schema: None,
            thinking: None,
            auto_spill_over: None,
        }
    }
}

pub(crate) const FINAL_ANSWER_TOOL: &str = "final_answer";

/// What can go wrong during a run.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The provider call failed (after retries).
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    /// The turn backstop tripped.
    #[error("exceeded max turns ({0})")]
    MaxTurnsExceeded(u32),
    /// A hook asked the loop to stop.
    #[error("halted by hook")]
    HookStop,
    /// The loop guard tripped its circuit breaker.
    #[error("loop guard circuit-broke: {0}")]
    CircuitBreak(String),
    #[error("media resolution failed: {0}")]
    Media(String),
    #[error("agent build failed: {0}")]
    Build(String),
}

/// Rewrites a request before the model call (e.g. swapping `artifact_ref` for
/// stored bytes). Applied once per call so fallback providers see it too;
/// `Err` fails the run.
#[async_trait::async_trait]
pub trait MediaResolver: Send + Sync {
    async fn resolve(&self, request: &mut CompletionRequest) -> Result<(), String>;
}

/// What one model turn produced — the orchestrator's per-iteration record.
#[derive(Debug)]
pub(crate) struct TurnRecord {
    /// Tool calls the model requested this turn (empty ⇒ the run is done).
    pub tool_calls: Vec<runic_types::ToolCall>,
    /// Why the model stopped this turn.
    pub stop_reason: runic_types::StopReason,
    /// Token usage for this turn's model call.
    pub usage: runic_types::TokenUsage,
    pub model: String,
    pub model_ms: u64,
}

/// The agent: a provider + a tool registry + hooks, driving an [`AgentState`].
pub struct Agent {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) fallbacks: Vec<FallbackProvider>,
    pub(crate) media_resolver: Option<Arc<dyn MediaResolver>>,
    pub(crate) tools: HashMap<String, Arc<dyn Tool>>,
    pub(crate) read_hooks: Vec<Arc<dyn ReadHook>>,
    pub(crate) write_hooks: Vec<Arc<dyn WriteHook>>,
    pub(crate) state: AgentState,
    pub(crate) config: AgentConfig,
    pub(crate) guard: loop_guard::LoopGuard,
    /// Live-event sink, installed per-run from [`RunContext`] (None for a
    /// non-streaming run).
    pub(crate) events: Option<mpsc::UnboundedSender<AgentEvent>>,
    /// Human channel, installed per-run from [`RunContext`] (None when no HITL
    /// surface is wired).
    pub(crate) human: Option<Arc<dyn HumanInterface>>,
    /// Boot-scoped resolver for on-demand tools (e.g. the deferred MCP
    /// catalog). Which tools are switched on lives in `state.data` under
    /// activation keys; `activated` below is this agent's materialization.
    pub(crate) catalog: Option<Arc<dyn ToolCatalog>>,
    pub(crate) activated: ActivatedToolSet,
    pub(crate) spill: Option<Arc<dyn ToolOutputSpill>>,
    pub(crate) child_persistence: Option<runic_state::ChildPersistenceHandle>,
    pub(crate) transient_tool_outputs: HashMap<String, Vec<serde_json::Value>>,
    pub(crate) pending_deferral: Option<PendingDeferral>,
    pub(crate) pending_external_tx: mpsc::UnboundedSender<runic_state::SessionEvent>,
    pub(crate) pending_external_rx: mpsc::UnboundedReceiver<runic_state::SessionEvent>,
}

impl Agent {
    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn max_turns(&self) -> u32 {
        self.config.max_turns
    }

    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }

    pub fn write_hook_names(&self) -> Vec<String> {
        self.write_hooks
            .iter()
            .map(|hook| hook.name().to_string())
            .collect()
    }

    /// Emit a live event if a streaming sink is attached for this run.
    pub(crate) fn emit(&self, event: AgentEvent) {
        if let Some(sink) = &self.events {
            let _ = sink.send(event);
        }
    }

    /// Materialize this conversation's activated tools from state: every
    /// `tool-search/activated/<name>` key resolves against the catalog once,
    /// so activations follow the thread through rebuilds and compaction.
    pub(crate) fn refresh_activated_tools(&mut self) {
        let Some(catalog) = &self.catalog else {
            return;
        };
        let names: Vec<String> = self
            .state
            .data()
            .iter()
            .filter(|(_, v)| v.as_bool().unwrap_or(false))
            .filter_map(|(k, _)| k.strip_prefix(ACTIVATED_KEY_PREFIX))
            .filter(|name| !self.activated.is_activated(name))
            .map(str::to_string)
            .collect();
        for name in names {
            match catalog.resolve(&name) {
                Some(tool) => self.activated.activate(name, tool),
                None => {
                    tracing::warn!(tool = %name, "activated tool missing from the catalog");
                }
            }
        }
    }

    /// Start building an agent for a `(user_id, session_id)` conversation.
    pub fn builder(
        provider: Arc<dyn Provider>,
        user_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> AgentBuilder {
        AgentBuilder::new(provider, user_id, session_id)
    }

    /// Borrow the underlying state.
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Mutably borrow the underlying state.
    pub fn state_mut(&mut self) -> &mut AgentState {
        &mut self.state
    }
}

/// Builder for [`Agent`].
pub struct AgentBuilder {
    provider: Arc<dyn Provider>,
    user_id: String,
    session_id: String,
    system_prompt: String,
    tools: Vec<Arc<dyn Tool>>,
    read_hooks: Vec<Arc<dyn ReadHook>>,
    write_hooks: Vec<Arc<dyn WriteHook>>,
    fallbacks: Vec<FallbackProvider>,
    media_resolver: Option<Arc<dyn MediaResolver>>,
    catalog: Option<Arc<dyn ToolCatalog>>,
    spill: Option<Arc<dyn ToolOutputSpill>>,
    config: AgentConfig,
}

impl AgentBuilder {
    fn new(
        provider: Arc<dyn Provider>,
        user_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            user_id: user_id.into(),
            session_id: session_id.into(),
            system_prompt: String::new(),
            tools: Vec::new(),
            read_hooks: Vec::new(),
            write_hooks: Vec::new(),
            fallbacks: Vec::new(),
            media_resolver: None,
            catalog: None,
            spill: None,
            config: AgentConfig::default(),
        }
    }

    /// Set the model identifier (passed to the provider).
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.config.model = model.into();
        self
    }

    /// Set the system prompt.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    /// Replace the full config.
    pub fn config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    /// Override the turn backstop.
    pub fn max_turns(mut self, n: u32) -> Self {
        self.config.max_turns = n;
        self
    }

    /// On hitting `max_turns`, make one final tools-free call instead of
    /// erroring.
    pub fn graceful_max_turns(mut self, graceful: bool) -> Self {
        self.config.graceful_max_turns = graceful;
        self
    }

    /// Register a tool.
    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Have the model deliver its final answer as JSON matching `schema` (via a
    /// synthetic `final_answer` tool). The result lands in [`RunOutcome::structured`].
    pub fn output_schema(mut self, schema: serde_json::Value) -> Self {
        self.config.output_schema = Some(schema);
        self
    }

    /// Add a fallback `(provider, model)`, tried in registration order when
    /// the primary model call fails with a not-found or persistent error.
    pub fn fallback(mut self, provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        self.fallbacks.push(FallbackProvider {
            provider,
            model: model.into(),
        });
        self
    }

    pub fn media_resolver(mut self, resolver: Arc<dyn MediaResolver>) -> Self {
        self.media_resolver = Some(resolver);
        self
    }

    /// Wire the artifact store used for `Retention::Artifact` tool outputs
    /// (and `auto_spill_over` when configured).
    pub fn artifact_spill(mut self, spill: Arc<dyn ToolOutputSpill>) -> Self {
        self.spill = Some(spill);
        self
    }

    /// Spill `Retention::Full` outputs above this many serialized bytes.
    pub fn auto_spill_over(mut self, bytes: usize) -> Self {
        self.config.auto_spill_over = Some(bytes);
        self
    }

    /// Wire the on-demand tool catalog (e.g. the deferred MCP set). An
    /// activating tool like `tool_search` records activations as state keys;
    /// the loop resolves them against this catalog each turn, so activations
    /// are per-conversation and survive rebuilds via the event log.
    pub fn tool_catalog(mut self, catalog: Arc<dyn ToolCatalog>) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// Register a read-only hook.
    pub fn read_hook(mut self, hook: Arc<dyn ReadHook>) -> Self {
        self.read_hooks.push(hook);
        self
    }

    /// Register a read-edit hook.
    pub fn write_hook(mut self, hook: Arc<dyn WriteHook>) -> Self {
        self.write_hooks.push(hook);
        self
    }

    /// Finish building. Hooks are sorted by `priority()` (lower runs first).
    pub fn build(mut self) -> Agent {
        let (pending_tx, pending_rx) = mpsc::unbounded_channel();
        let state = AgentState::new(self.user_id, self.session_id, self.system_prompt);
        let tools = self
            .tools
            .into_iter()
            .map(|t| (t.name().to_string(), t))
            .collect();
        self.read_hooks.sort_by_key(|h| h.priority());
        self.write_hooks.sort_by_key(|h| h.priority());
        Agent {
            provider: self.provider,
            fallbacks: self.fallbacks,
            media_resolver: self.media_resolver,
            tools,
            read_hooks: self.read_hooks,
            write_hooks: self.write_hooks,
            state,
            config: self.config,
            guard: loop_guard::LoopGuard::default(),
            events: None,
            human: None,
            catalog: self.catalog,
            activated: ActivatedToolSet::default(),
            spill: self.spill,
            child_persistence: None,
            transient_tool_outputs: HashMap::new(),
            pending_deferral: None,
            pending_external_tx: pending_tx,
            pending_external_rx: pending_rx,
        }
    }
}
