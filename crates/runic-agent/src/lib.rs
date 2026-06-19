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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use runic_hook::{ReadHook, WriteHook};
use runic_provider::{Provider, ProviderError};
use runic_state::AgentState;
use runic_tool::{ActivatedToolSet, HumanInterface, Tool};
use tokio::sync::mpsc;

mod run;
mod turn;

pub mod loop_guard;
pub mod retry;

pub use runic_state::RunOutcome;

/// Default hard cap on model turns per run — a backstop against runaway loops
/// (the tunable policy lives in [`loop_guard`] and hooks).
pub const DEFAULT_MAX_TURNS: u32 = 64;
/// Default per-tool execution timeout.
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;
/// Default max output tokens per model call.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

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
    /// A run began.
    RunStarted { run_id: String },
    /// Incremental assistant text.
    TextDelta(String),
    /// Incremental reasoning/thinking text.
    ThinkingDelta(String),
    /// A tool is about to run.
    ToolStarted { id: String, name: String },
    /// A tool finished.
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
    },
    /// One model turn completed.
    TurnCompleted { turn: u32, stop_reason: String },
    /// The run finished.
    RunCompleted(RunOutcome),
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
}

/// Tunable knobs for a run.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Model identifier passed to the provider.
    pub model: String,
    /// Max output tokens per model call.
    pub max_tokens: u32,
    /// Sampling temperature.
    pub temperature: f32,
    /// Hard backstop on model turns per run.
    pub max_turns: u32,
    /// Per-tool execution timeout.
    pub tool_timeout: Duration,
    /// On hitting `max_turns`: if `true`, make one final tools-free call to
    /// extract a best-effort answer; if `false`, error out.
    pub graceful_max_turns: bool,
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
        }
    }
}

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
}

/// The agent: a provider + a tool registry + hooks, driving an [`AgentState`].
pub struct Agent {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) fallbacks: Vec<FallbackProvider>,
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
    /// Tools activated on demand this conversation (e.g. via an MCP
    /// `tool_search`). The loop adds their specs to each request and resolves
    /// calls against them. Shared with the activating tool.
    pub(crate) activated: Option<Arc<Mutex<ActivatedToolSet>>>,
}

impl Agent {
    /// Emit a live event if a streaming sink is attached for this run.
    pub(crate) fn emit(&self, event: AgentEvent) {
        if let Some(sink) = &self.events {
            let _ = sink.send(event);
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
    activated: Option<Arc<Mutex<ActivatedToolSet>>>,
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
            activated: None,
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

    /// Add a fallback `(provider, model)`, tried in registration order when
    /// the primary model call fails with a not-found or persistent error.
    pub fn fallback(mut self, provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        self.fallbacks.push(FallbackProvider {
            provider,
            model: model.into(),
        });
        self
    }

    /// Share an [`ActivatedToolSet`] for on-demand tool activation (e.g. an
    /// MCP `tool_search` registered via [`AgentBuilder::tool`] writes into the
    /// same set the loop reads). Enables deferred/lazy tools.
    pub fn activated_tools(mut self, activated: Arc<Mutex<ActivatedToolSet>>) -> Self {
        self.activated = Some(activated);
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
            tools,
            read_hooks: self.read_hooks,
            write_hooks: self.write_hooks,
            state,
            config: self.config,
            guard: loop_guard::LoopGuard::default(),
            events: None,
            human: None,
            activated: self.activated,
        }
    }
}
