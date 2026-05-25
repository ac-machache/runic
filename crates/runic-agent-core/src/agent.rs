use chrono::Utc;
use futures::StreamExt;
use runic_context_engine::{ContextEngine, NoopEngine, TurnContext};
use runic_message_types::{ContentBlock, Message, Role, ToolCall};
use runic_provider_core::Provider;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::error::AgentError;
use crate::event::{AgentEvent, TokenUsage};
use crate::hooks::{Hook, HookOutcome};
use crate::state::{AgentState, HookLifecycle, RunTimeContext, SessionEvent};
use runic_tool_core::{ToolContext, ToolDispatchError, ToolRegistry, ToolResult};

/// Tunable knobs for the agent run loop.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Hard cap on model turns within a single `run` call.
    pub max_turns: u32,
    /// Emit timestamp prefixes to user-role text/tool_result blocks before
    /// sending to the provider. Helpful when the model relies on wall-clock.
    pub stamp_messages: bool,
    /// When non-empty, sent as `system_dynamic` via `Provider::complete_split`
    /// so it does not perturb the cached `system_prompt` prefix.
    pub system_dynamic: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 64,
            stamp_messages: false,
            system_dynamic: String::new(),
        }
    }
}

/// Final outcome of a `run` call once the loop has terminated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOutcome {
    pub total_turns: u32,
    pub stop_reason: Option<String>,
    pub usage: TokenUsage,
}

/// Returned by `Agent::run_streaming`: the event channel plus a join handle
/// that yields the consumed `Agent` and the final `RunOutcome` once the loop
/// terminates.
pub type RunStreamingHandle = (
    ReceiverStream<AgentEvent>,
    tokio::task::JoinHandle<(Agent, Result<RunOutcome, AgentError>)>,
);

/// The agent kernel.
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    hooks: Vec<Arc<dyn Hook>>,
    config: AgentConfig,
    state: AgentState,
    /// Optional context engine. Defaults to `NoopEngine` (identity for all
    /// methods), which means: use the static system prompt as-is, no
    /// ambient injections, no spillover, no compaction. Configure with
    /// `AgentBuilder::context_engine`.
    context_engine: Arc<dyn ContextEngine>,
}

impl Agent {
    pub fn builder(provider: Arc<dyn Provider>) -> AgentBuilder {
        AgentBuilder::new(provider)
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut AgentState {
        &mut self.state
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Subscribe to the agent's [`crate::state::SessionEvent`] stream.
    ///
    /// Every event pushed to state (run start/end, message, tool call,
    /// hook fired, state snapshot, …) is broadcast to every subscriber.
    /// External consumers — a persister writing to a SessionStore, an
    /// observability sink, a UI mirror — can each hold their own
    /// `Receiver` without coordinating.
    ///
    /// Channel capacity is [`crate::state::EVENT_BROADCAST_CAPACITY`].
    /// If a subscriber falls behind, it sees `RecvError::Lagged(n)` on
    /// the next `recv` — explicit, never silent.
    pub fn subscribe_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::state::SessionEvent> {
        self.state
            .subscribe_events()
            .expect("AgentBuilder must install events_tx in build()")
    }

    /// Drive a user input to completion, streaming events.
    pub fn run_streaming(mut self, user_input: impl Into<String>) -> RunStreamingHandle {
        let (tx, rx) = mpsc::channel::<AgentEvent>(128);
        let user_input = user_input.into();
        let handle = tokio::spawn(async move {
            let result = self.run_inner(user_input, &tx).await;
            (self, result)
        });
        (ReceiverStream::new(rx), handle)
    }

    /// Convenience: drive a user input to completion, discarding intermediate events.
    pub async fn run(&mut self, user_input: impl Into<String>) -> Result<RunOutcome, AgentError> {
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = self.run_inner(user_input.into(), &tx).await;
        drop(tx);
        let _ = drain.await;
        result
    }

    async fn run_inner(
        &mut self,
        user_input: String,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<RunOutcome, AgentError> {
        let run_id = uuid::Uuid::new_v4().to_string();

        self.state.push_event(SessionEvent::RunStart {
            run_id: run_id.clone(),
            at: Utc::now(),
        });

        // Engine pass: process_user_input — engine may rewrite the message
        // (e.g. spill huge pastes to disk, redact secrets, attach metadata).
        let raw_user_msg = Message::user(&user_input);
        let processed_user_msg = {
            let messages_so_far = self.state.messages_for_provider();
            let ctx = TurnContext {
                base_system_prompt: &self.state.system_prompt,
                messages: &messages_so_far,
                run_id: &run_id,
                turn: 0,
            };
            self.context_engine
                .process_user_input(&ctx, raw_user_msg)
                .await
        };
        self.state.push_event(SessionEvent::Message {
            run_id: run_id.clone(),
            msg: processed_user_msg,
            at: Utc::now(),
        });

        if matches!(
            self.run_lifecycle_hooks(HookLifecycle::BeforeAgent, &run_id, |h, s| Box::pin(
                async move { h.before_agent(s).await }
            ))
            .await?,
            HookOutcome::Stop
        ) {
            return Err(AgentError::HookStop);
        }

        let mut total_turns: u32 = 0;
        let mut total_usage = TokenUsage::default();
        let final_stop_reason: Option<String>;

        loop {
            if total_turns >= self.config.max_turns {
                return Err(AgentError::MaxTurnsExceeded(self.config.max_turns));
            }

            if matches!(
                self.run_lifecycle_hooks(HookLifecycle::BeforeModel, &run_id, |h, s| Box::pin(
                    async move { h.before_model(s).await }
                ))
                .await?,
                HookOutcome::Stop
            ) {
                return Err(AgentError::HookStop);
            }

            let TurnRecord {
                assistant_message,
                tool_calls,
                stop_reason,
                usage,
            } = self.run_one_turn(&run_id, total_turns + 1, events).await?;

            total_turns += 1;
            merge_usage(&mut total_usage, &usage);

            self.state.push_event(SessionEvent::Message {
                run_id: run_id.clone(),
                msg: assistant_message,
                at: Utc::now(),
            });
            self.state.push_event(SessionEvent::TurnBoundary {
                run_id: run_id.clone(),
                at: Utc::now(),
            });

            let _ = events
                .send(AgentEvent::TurnComplete {
                    stop_reason: stop_reason.clone(),
                    tool_calls_this_turn: tool_calls.len() as u32,
                })
                .await;

            let stop_reason_for_hook = stop_reason.clone();
            if matches!(
                self.run_lifecycle_hooks(HookLifecycle::AfterModel, &run_id, move |h, s| {
                    let sr = stop_reason_for_hook.clone();
                    Box::pin(async move { h.after_model(s, sr.as_deref()).await })
                })
                .await?,
                HookOutcome::Stop
            ) {
                return Err(AgentError::HookStop);
            }

            if tool_calls.is_empty() {
                final_stop_reason = stop_reason;
                break;
            }

            self.dispatch_tools(tool_calls, &run_id, total_turns, events)
                .await?;
        }

        if matches!(
            self.run_lifecycle_hooks(HookLifecycle::AfterAgent, &run_id, |h, s| Box::pin(
                async move { h.after_agent(s).await }
            ))
            .await?,
            HookOutcome::Stop
        ) {
            return Err(AgentError::HookStop);
        }

        let outcome = RunOutcome {
            total_turns,
            stop_reason: final_stop_reason,
            usage: total_usage,
        };

        self.state.push_event(SessionEvent::RunEnd {
            run_id: run_id.clone(),
            outcome: outcome.clone(),
            at: Utc::now(),
        });

        let _ = events.send(AgentEvent::RunComplete { total_turns }).await;

        Ok(outcome)
    }

    async fn run_one_turn(
        &mut self,
        run_id: &str,
        turn: u32,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<TurnRecord, AgentError> {
        use runic_message_types::StreamEvent;

        let definitions = self.tools.definitions();
        let provider_messages = self.state.messages_for_provider();
        let mut messages_owned: Vec<Message> = if self.config.stamp_messages {
            Message::with_timestamps(&provider_messages)
        } else {
            provider_messages
        };

        // Engine pass: maybe_compact — own scope so the &mut messages_owned
        // borrow does not conflict with later &messages_owned borrows.
        {
            let ctx_for_compact = TurnContext {
                base_system_prompt: &self.state.system_prompt,
                messages: &[],
                run_id,
                turn,
            };
            self.context_engine
                .maybe_compact(&ctx_for_compact, &mut messages_owned)
                .await;
        }

        // Engine pass: assemble system prompt + collect ambient notes.
        let (system_prompt, ambient_dynamic) = {
            let ctx = TurnContext {
                base_system_prompt: &self.state.system_prompt,
                messages: &messages_owned,
                run_id,
                turn,
            };
            let sp = self.context_engine.assemble_system_prompt(&ctx).await;
            let notes = self.context_engine.ambient_notes(&ctx).await;
            let dyn_text = notes
                .into_iter()
                .map(|n| n.content)
                .collect::<Vec<_>>()
                .join("\n\n");
            (sp, dyn_text)
        };

        // Merge config's static system_dynamic with engine's ambient text.
        let combined_dynamic = match (
            self.config.system_dynamic.trim().is_empty(),
            ambient_dynamic.trim().is_empty(),
        ) {
            (true, true) => String::new(),
            (false, true) => self.config.system_dynamic.clone(),
            (true, false) => ambient_dynamic,
            (false, false) => format!("{}\n\n{}", self.config.system_dynamic, ambient_dynamic),
        };

        let stream = if combined_dynamic.is_empty() {
            self.provider
                .complete(&messages_owned, &definitions, &system_prompt, None)
                .await?
        } else {
            self.provider
                .complete_split(
                    &messages_owned,
                    &definitions,
                    &system_prompt,
                    &combined_dynamic,
                    None,
                )
                .await?
        };

        let mut blocks: Vec<PendingBlock> = Vec::new();
        let mut stop_reason: Option<String> = None;
        let mut usage = TokenUsage::default();
        let mut error_message: Option<String> = None;

        tokio::pin!(stream);
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::TextDelta(text)) => {
                    let _ = events
                        .send(AgentEvent::AssistantTextDelta(text.clone()))
                        .await;
                    append_text(&mut blocks, &text);
                }
                Ok(StreamEvent::ThinkingStart) => {
                    blocks.push(PendingBlock::Thinking(String::new()));
                }
                Ok(StreamEvent::ThinkingDelta(text)) => {
                    let _ = events
                        .send(AgentEvent::AssistantThinkingDelta(text.clone()))
                        .await;
                    if let Some(PendingBlock::Thinking(buf)) = blocks.last_mut() {
                        buf.push_str(&text);
                    } else {
                        blocks.push(PendingBlock::Thinking(text));
                    }
                }
                Ok(StreamEvent::ThinkingEnd) => {}
                Ok(StreamEvent::ThinkingDone { .. }) => {}
                Ok(StreamEvent::ToolUseStart { id, name }) => {
                    let _ = events
                        .send(AgentEvent::ToolUseStart {
                            id: id.clone(),
                            name: name.clone(),
                        })
                        .await;
                    blocks.push(PendingBlock::ToolUse {
                        id,
                        name,
                        input_buf: String::new(),
                    });
                }
                Ok(StreamEvent::ToolInputDelta(fragment)) => {
                    if let Some(PendingBlock::ToolUse { input_buf, .. }) = blocks.last_mut() {
                        input_buf.push_str(&fragment);
                    }
                }
                Ok(StreamEvent::ToolUseEnd) => {}
                Ok(StreamEvent::ToolResult { .. }) => {}
                Ok(StreamEvent::MessageEnd { stop_reason: sr }) => {
                    stop_reason = sr;
                }
                Ok(StreamEvent::TokenUsage {
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                }) => {
                    let snapshot = TokenUsage {
                        input_tokens,
                        output_tokens,
                        cache_read_input_tokens,
                        cache_creation_input_tokens,
                    };
                    merge_usage(&mut usage, &snapshot);
                    let _ = events.send(AgentEvent::Usage(snapshot)).await;
                }
                Ok(StreamEvent::Error {
                    message,
                    retry_after_secs: _,
                }) => {
                    error_message = Some(message);
                    break;
                }
                Ok(StreamEvent::SessionId(_))
                | Ok(StreamEvent::ConnectionType { .. })
                | Ok(StreamEvent::ConnectionPhase { .. })
                | Ok(StreamEvent::StatusDetail { .. })
                | Ok(StreamEvent::Compaction { .. }) => {}
                Err(err) => {
                    return Err(AgentError::Provider(err));
                }
            }
        }

        if let Some(message) = error_message {
            return Err(AgentError::Internal(format!(
                "provider stream error: {message}"
            )));
        }

        let (assistant_message, tool_calls) = finalize_blocks(blocks);

        Ok(TurnRecord {
            assistant_message,
            tool_calls,
            stop_reason,
            usage,
        })
    }

    async fn dispatch_tools(
        &mut self,
        calls: Vec<ToolCall>,
        run_id: &str,
        turn: u32,
        events: &mpsc::Sender<AgentEvent>,
    ) -> Result<(), AgentError> {
        if calls.is_empty() {
            return Ok(());
        }

        let bag = self.state.runtime.snapshot();
        let ctx = ToolContext::new(
            self.state.session_id.clone(),
            run_id.to_string(),
            turn,
            bag,
        );

        // ─── Phase 1: serial before_tool hooks + per-call planning ──────────
        let mut plans: Vec<CallPlan> = Vec::with_capacity(calls.len());

        for mut call in calls {
            let mut substituted: Option<ToolResult> = None;
            for hook in self.hooks.clone() {
                let hook_name = hook.name().to_string();
                let outcome = hook.before_tool(&mut self.state, &mut call).await;
                self.state.push_event(SessionEvent::HookRan {
                    run_id: run_id.to_string(),
                    hook: hook_name,
                    lifecycle: HookLifecycle::BeforeTool,
                    note: None,
                    at: Utc::now(),
                });
                match outcome {
                    HookOutcome::Continue => {}
                    HookOutcome::SubstituteToolResult(result) => {
                        substituted = Some(result);
                        break;
                    }
                    HookOutcome::Stop => return Err(AgentError::HookStop),
                }
            }

            let plan = match substituted {
                Some(result) => CallPlan::Substituted { call, result },
                None => {
                    let parallelizable = self
                        .tools
                        .get(&call.name)
                        .map(|t| t.parallelizable())
                        .unwrap_or(false);
                    CallPlan::Dispatch {
                        call,
                        parallelizable,
                    }
                }
            };
            plans.push(plan);
        }

        // ─── Phase 2: parallel dispatch where safe, serial for the rest ─────
        let n = plans.len();
        let mut results: Vec<Option<(ToolCall, ToolResult, u64)>> =
            (0..n).map(|_| None).collect();

        // Pre-fill already-substituted slots AND emit their lifecycle events
        // so the consumer treats them identically to a real dispatch (the fact
        // that a hook substituted is transparent to the UI).
        for (i, plan) in plans.iter().enumerate() {
            if let CallPlan::Substituted { call, result } = plan {
                let _ = events.send(AgentEvent::ToolDispatching(call.clone())).await;
                let _ = events
                    .send(AgentEvent::ToolFinished {
                        call: call.clone(),
                        result: result.clone(),
                        duration_ms: 0,
                    })
                    .await;
                results[i] = Some((call.clone(), result.clone(), 0));
            }
        }

        // Emit ToolDispatching events in input order so the live stream reflects
        // "we just kicked these off" before any complete out-of-order.
        for plan in &plans {
            if let CallPlan::Dispatch { call, .. } = plan {
                let _ = events.send(AgentEvent::ToolDispatching(call.clone())).await;
            }
        }

        // Parallel batch — all parallelizable Dispatch entries run via join_all.
        let parallel_indices: Vec<usize> = plans
            .iter()
            .enumerate()
            .filter_map(|(i, p)| match p {
                CallPlan::Dispatch {
                    parallelizable: true,
                    ..
                } => Some(i),
                _ => None,
            })
            .collect();

        if !parallel_indices.is_empty() {
            let tools_ref = &self.tools;
            let ctx_ref = &ctx;
            let parallel_futures = parallel_indices.iter().map(|&i| {
                let call = match &plans[i] {
                    CallPlan::Dispatch { call, .. } => call.clone(),
                    _ => unreachable!(),
                };
                let events = events.clone();
                async move {
                    let started = Instant::now();
                    let result = match tools_ref.dispatch(&call, ctx_ref).await {
                        Ok(r) => r,
                        Err(ToolDispatchError::UnknownTool { tool }) => {
                            ToolResult::error(format!("unknown tool: {tool}"))
                        }
                    };
                    let duration_ms = started.elapsed().as_millis() as u64;
                    let _ = events
                        .send(AgentEvent::ToolFinished {
                            call: call.clone(),
                            result: result.clone(),
                            duration_ms,
                        })
                        .await;
                    (i, call, result, duration_ms)
                }
            });
            for (i, call, result, dur) in futures::future::join_all(parallel_futures).await {
                results[i] = Some((call, result, dur));
            }
        }

        // Sequential batch — HitlTools (or anything else that opted out).
        for (i, plan) in plans.iter().enumerate() {
            if !matches!(
                plan,
                CallPlan::Dispatch {
                    parallelizable: false,
                    ..
                }
            ) {
                continue;
            }
            let call = match plan {
                CallPlan::Dispatch { call, .. } => call.clone(),
                _ => unreachable!(),
            };
            let started = Instant::now();
            let result = match self.tools.dispatch(&call, &ctx).await {
                Ok(r) => r,
                Err(ToolDispatchError::UnknownTool { tool }) => {
                    ToolResult::error(format!("unknown tool: {tool}"))
                }
            };
            let duration_ms = started.elapsed().as_millis() as u64;
            let _ = events
                .send(AgentEvent::ToolFinished {
                    call: call.clone(),
                    result: result.clone(),
                    duration_ms,
                })
                .await;
            results[i] = Some((call, result, duration_ms));
        }

        // ─── Phase 3: serial after_tool hooks + audit (in input order) ──────
        let mut tool_result_blocks: Vec<ContentBlock> = Vec::with_capacity(n);
        let mut total_duration_ms: u64 = 0;

        for slot in results.iter_mut() {
            let (call, result, duration_ms) =
                slot.take().expect("phase 2 must fill every slot");
            total_duration_ms = total_duration_ms.saturating_add(duration_ms);

            tool_result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: result.content.clone(),
                is_error: if result.is_error { Some(true) } else { None },
            });

            for hook in self.hooks.clone() {
                let hook_name = hook.name().to_string();
                let outcome = hook.after_tool(&mut self.state, &call, &result).await;
                self.state.push_event(SessionEvent::HookRan {
                    run_id: run_id.to_string(),
                    hook: hook_name,
                    lifecycle: HookLifecycle::AfterTool,
                    note: None,
                    at: Utc::now(),
                });
                match outcome {
                    HookOutcome::Continue => {}
                    HookOutcome::SubstituteToolResult(_) => {}
                    HookOutcome::Stop => return Err(AgentError::HookStop),
                }
            }
        }

        let user_msg = Message {
            role: Role::User,
            content: tool_result_blocks,
            timestamp: Some(Utc::now()),
            tool_duration_ms: Some(total_duration_ms),
        };
        self.state.push_event(SessionEvent::Message {
            run_id: run_id.to_string(),
            msg: user_msg,
            at: Utc::now(),
        });

        Ok(())
    }

    /// Run a single hook lifecycle across all registered hooks, recording a
    /// `HookRan` event for each. Short-circuits on `Stop`.
    async fn run_lifecycle_hooks<F>(
        &mut self,
        lifecycle: HookLifecycle,
        run_id: &str,
        mut f: F,
    ) -> Result<HookOutcome, AgentError>
    where
        for<'a> F:
            FnMut(Arc<dyn Hook>, &'a mut AgentState) -> futures::future::BoxFuture<'a, HookOutcome>,
    {
        for hook in self.hooks.clone() {
            let hook_name = hook.name().to_string();
            let outcome = f(hook.clone(), &mut self.state).await;
            self.state.push_event(SessionEvent::HookRan {
                run_id: run_id.to_string(),
                hook: hook_name,
                lifecycle,
                note: None,
                at: Utc::now(),
            });
            match outcome {
                HookOutcome::Continue => {}
                HookOutcome::SubstituteToolResult(_) => {}
                HookOutcome::Stop => return Ok(HookOutcome::Stop),
            }
        }
        Ok(HookOutcome::Continue)
    }
}

/// Per-call decision made in phase 1 of `dispatch_tools`.
enum CallPlan {
    /// A hook substituted a synthetic result; skip the dispatch step.
    Substituted { call: ToolCall, result: ToolResult },
    /// Run the tool. `parallelizable` is sourced from the adapter and
    /// decides whether it joins the parallel batch or runs serially.
    Dispatch {
        call: ToolCall,
        parallelizable: bool,
    },
}

struct TurnRecord {
    assistant_message: Message,
    tool_calls: Vec<ToolCall>,
    stop_reason: Option<String>,
    usage: TokenUsage,
}

enum PendingBlock {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        input_buf: String,
    },
}

fn append_text(blocks: &mut Vec<PendingBlock>, text: &str) {
    if let Some(PendingBlock::Text(buf)) = blocks.last_mut() {
        buf.push_str(text);
    } else {
        blocks.push(PendingBlock::Text(text.to_string()));
    }
}

fn finalize_blocks(blocks: Vec<PendingBlock>) -> (Message, Vec<ToolCall>) {
    let mut content: Vec<ContentBlock> = Vec::with_capacity(blocks.len());
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in blocks {
        match block {
            PendingBlock::Text(text) => content.push(ContentBlock::Text {
                text,
                cache_control: None,
            }),
            PendingBlock::Thinking(text) => content.push(ContentBlock::Reasoning { text }),
            PendingBlock::ToolUse {
                id,
                name,
                input_buf,
            } => {
                let input: serde_json::Value = if input_buf.trim().is_empty() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&input_buf)
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()))
                };
                content.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                tool_calls.push(ToolCall {
                    id,
                    name,
                    input,
                    intent: None,
                });
            }
        }
    }

    let message = Message {
        role: Role::Assistant,
        content,
        timestamp: Some(Utc::now()),
        tool_duration_ms: None,
    };

    (message, tool_calls)
}

fn merge_usage(acc: &mut TokenUsage, next: &TokenUsage) {
    fn add(target: &mut Option<u64>, source: Option<u64>) {
        if let Some(v) = source {
            *target = Some(target.unwrap_or(0) + v);
        }
    }
    add(&mut acc.input_tokens, next.input_tokens);
    add(&mut acc.output_tokens, next.output_tokens);
    add(
        &mut acc.cache_read_input_tokens,
        next.cache_read_input_tokens,
    );
    add(
        &mut acc.cache_creation_input_tokens,
        next.cache_creation_input_tokens,
    );
}

/// Fluent builder for `Agent`.
pub struct AgentBuilder {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    hooks: Vec<Arc<dyn Hook>>,
    config: AgentConfig,
    system_prompt: String,
    session_id: Option<String>,
    runtime: RunTimeContext,
    context_engine: Arc<dyn ContextEngine>,
}

impl AgentBuilder {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            tools: ToolRegistry::new(),
            hooks: Vec::new(),
            config: AgentConfig::default(),
            system_prompt: String::new(),
            session_id: None,
            runtime: RunTimeContext::default(),
            context_engine: Arc::new(NoopEngine),
        }
    }

    /// Plug in a `ContextEngine` to manage the system prompt, ambient
    /// reminders, compaction, and user-input preprocessing. Defaults to
    /// `NoopEngine` which preserves the original behaviour (static system
    /// prompt, no compaction, no reminders).
    pub fn context_engine<E: ContextEngine + 'static>(mut self, engine: E) -> Self {
        self.context_engine = Arc::new(engine);
        self
    }

    /// Same as [`Self::context_engine`] but accepts an already-shared
    /// engine. Useful when the engine is a chain of decorators built up
    /// behind `Arc<dyn ContextEngine>` (e.g. `Spillover -> Compactor ->
    /// Composite`) — that shape doesn't fit the generic-impl bound on
    /// [`Self::context_engine`] but is exactly what wrapping engines
    /// produce.
    pub fn context_engine_arc(mut self, engine: Arc<dyn ContextEngine>) -> Self {
        self.context_engine = engine;
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn tool<T: runic_tool_core::Tool + 'static>(mut self, tool: Arc<T>) -> Self {
        self.tools.register(tool);
        self
    }

    pub fn hitl_tool<T: runic_tool_core::HitlTool + 'static>(mut self, tool: Arc<T>) -> Self {
        self.tools.register_hitl(tool);
        self
    }

    /// Register a long-running tool. On first use this also auto-installs the
    /// `BackgroundManager` in runtime context and registers the generic
    /// `background_status` tool so the model can poll task ids.
    pub fn background_tool<T: runic_tool_core::BackgroundTool + 'static>(
        mut self,
        tool: Arc<T>,
    ) -> Self {
        if self
            .runtime
            .get::<runic_tool_core::BackgroundManager>()
            .is_none()
        {
            self.runtime
                .insert(runic_tool_core::BackgroundManager::new());
            self.tools
                .register(Arc::new(runic_tool_core::BackgroundStatusTool));
            self.tools
                .register(Arc::new(runic_tool_core::BackgroundCancelTool));
        }
        self.tools.register_background(tool);
        self
    }

    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    pub fn hook(mut self, hook: Arc<dyn Hook>) -> Self {
        self.hooks.push(hook);
        self
    }

    /// Stash a typed handle that tools and hooks can fetch via
    /// `ctx.get::<T>()` / `state.runtime.get::<T>()`. The model never sees it.
    pub fn runtime<T: 'static + Send + Sync>(mut self, value: T) -> Self {
        self.runtime.insert(value);
        self
    }

    /// Same as [`Self::runtime`] but accepts an already-shared `Arc<T>`.
    /// Lets callers keep their own clone alive after handing one to the
    /// agent — e.g. sharing a `BackgroundManager` with a reminder.
    pub fn runtime_arc<T: 'static + Send + Sync>(mut self, value: Arc<T>) -> Self {
        self.runtime.insert_arc(value);
        self
    }

    /// Convenience: register an externally-constructed `BackgroundManager`
    /// AND the `background_status` / `background_cancel` helper tools that
    /// usually get auto-installed by the first [`Self::background_tool`]
    /// call. Use this when something outside the agent (a
    /// `BackgroundTaskReminder`, monitoring code) needs to share the
    /// manager.
    pub fn background_manager(
        mut self,
        manager: Arc<runic_tool_core::BackgroundManager>,
    ) -> Self {
        self.runtime.insert_arc(manager);
        self.tools
            .register(Arc::new(runic_tool_core::BackgroundStatusTool));
        self.tools
            .register(Arc::new(runic_tool_core::BackgroundCancelTool));
        self
    }

    pub fn build(self) -> Agent {
        let session_id = self
            .session_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut state = AgentState::new(session_id, self.system_prompt);
        state.runtime = self.runtime;

        // Install the broadcast channel. Every Agent has one from birth,
        // regardless of whether anyone subscribes — sends with no
        // subscribers cost a single Err that we ignore.
        let (events_tx, _initial_rx) =
            tokio::sync::broadcast::channel::<crate::state::SessionEvent>(
                crate::state::EVENT_BROADCAST_CAPACITY,
            );
        state.set_events_tx(events_tx);

        Agent {
            provider: self.provider,
            tools: self.tools,
            hooks: self.hooks,
            config: self.config,
            state,
            context_engine: self.context_engine,
        }
    }
}
