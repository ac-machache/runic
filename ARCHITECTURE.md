# Architecture

This document explains the design of runic: what each crate does, how they
fit together, and how a single turn flows through the system.

## Design principles

1. **Headless library first.** No TUI, no CLI, no UI in the core. The
   `runic` binary is a sample REPL; build your own surface on top of the
   library.

2. **Traits, not enums, for cross-cutting concerns.** Storage, providers,
   transports, context engines, tools — all are traits with multiple
   implementations. Adding a new variant is a new `impl`, not a closed-set
   edit.

3. **Decorator pattern for behavior composition.** Context engines wrap
   each other (Composite ← Compactor ← Spillover ← Reminder). Adding a new
   behavior layer doesn't touch existing engines.

4. **Pure data registries.** `SkillRegistry`, `AgentRegistry`, etc. are
   in-memory data structures with no retained backend reference after
   `load()`. Trivial to test, trivial to clone, no surprises.

5. **No model output leaves the engine without going through hooks.**
   Six hook points let you intercept, log, mutate, or veto anything the
   loop does.

## Crate dependency graph

```
                                 ┌─────────────────────┐
                                 │ runic-message-types │
                                 └──────────┬──────────┘
              ┌───────────────────┬──────────┴────────┬───────────────┐
              │                   │                   │               │
   ┌──────────▼──────────┐  ┌─────▼──────────┐  ┌────▼────────┐ ┌────▼────────────┐
   │ runic-provider-core │  │ runic-tool-core│  │ runic-storage│ │ (used by all)   │
   └──────────┬──────────┘  └─────┬──────────┘  │   -backend   │ │                 │
              │                   │              └────┬────────┘ │                 │
   ┌──────────┴───────────┐       │                   │           │                 │
   │                      │       │                   │           │                 │
┌──▼─────────┐   ┌────────▼┐      │                   │           │                 │
│ provider-  │   │ provider│      │                   │           │                 │
│ anthropic  │   │ -gemini │      │                   │           │                 │
└──────┬─────┘   └───┬─────┘      │                   │           │                 │
       │             │            │                   │           │                 │
       │             │            │   ┌────────────────────────────────────────┐    │
       │             │            │   │      runic-context-engine              │    │
       │             │            │   │  (depends on tool-core, storage,       │    │
       │             │            │   │   provider-core, message-types)        │    │
       │             │            │   └────────┬───────────────────────────────┘    │
       │             │            │            │                                    │
       └─────────────┴────────────┴────────────┴───────────────────────────────┐    │
                                                                               │    │
                                                          ┌────────────────────▼────▼─┐
                                                          │     runic-agent-core      │
                                                          │ (Agent, hooks, state,     │
                                                          │  SubagentTool)            │
                                                          └─┬─────────────────────────┘
                                                            │
        ┌────────────────────────────┬──────────────┬──────┴───────┬─────────────────────┐
        │                            │              │              │                     │
┌───────▼──────┐  ┌─────────▼──┐  ┌──▼────────┐  ┌─▼──────────┐  ┌▼──────────────┐
│ runic-skills │  │ runic-agents │  │ runic-mcp │  │ runic-     │  │ runic         │
│ (SKILL.md +  │  │ (AGENT.md +  │  │ (stdio +  │  │  plugins   │  │  (REPL binary │
│  registry +  │  │  registry +  │  │  http     │  │ (bundles)  │  │   that wires  │
│  layer +tool)│  │  conversion) │  │  client)  │  │            │  │   everything) │
└──────────────┘  └──────────────┘  └───────────┘  └────────────┘  └───────────────┘
```

Rules baked into the DAG:
- `runic-tool-core` has no `runic-*` deps. Tools can be defined without
  pulling in the agent loop.
- `runic-context-engine` doesn't depend on `runic-agent-core` — engines
  are reusable outside the agent.
- Skills, agents, mcp, and plugins are **sibling extension crates**.
  Each one slots into the agent without the others knowing.

## A single turn, end to end

When you type a message into the REPL:

```
1. User input → agent.run_streaming("text")
2. ContextEngine::process_user_input(msg)
   ├─ Inner engines first, then outer wrappers
   └─ Returns a (possibly rewritten) Message that goes into state
3. Hooks: before_agent (once per run)
4. Turn loop begins:
   a. Hooks: before_model
   b. Build provider request:
      ├─ ContextEngine::maybe_compact(messages)         ← Compactor + Spillover
      ├─ ContextEngine::assemble_system_prompt(ctx)     ← Composite (layers)
      └─ ContextEngine::ambient_notes(ctx)              ← Reminder
   c. Provider::complete_split(messages, tools, system, dynamic)
      ├─ Anthropic SSE / Gemini SSE / etc.
      └─ Stream events: TextDelta, ToolUseStart, MessageEnd, ...
   d. Stream consumed → assistant Message constructed
   e. Hooks: after_model
   f. If tool calls present:
      - Hooks: before_tool (per call, serial)
      - Dispatch:
        ├─ Parallelizable tools join_all'd
        └─ Non-parallelizable (HITL) serialized
      - Hooks: after_tool (per call, serial)
      - Tool results pushed into state as a User Message
   g. If no tool calls: loop ends
5. Hooks: after_agent
6. Run complete
```

Every step in 4(b–c) is interceptable via either a hook or a
ContextEngine method.

## Context engine pipeline (the heart of the design)

The agent holds ONE `Arc<dyn ContextEngine>`, but that single engine is
usually a stack of decorators:

```
ReminderEngine          ← outermost; adds peripheral-vision notes
   ↓ wraps
SpilloverEngine         ← writes huge tool results to disk
   ↓ wraps
CompactorEngine         ← summarizes old messages if total > threshold
   ↓ wraps
CompositeEngine         ← innermost; assembles system prompt from layers
   ↓ holds
[BasePromptLayer, PersonaLayer, UserFactsLayer, MemoryLayer, SkillsIndexLayer]
```

Each engine implements `ContextEngine` and delegates to its inner.
Adding a new behavior = a new decorator engine. Removing one = unwrap it
in the binary's `main.rs`.

See [docs/context-engine.md](./docs/context-engine.md) for the full API.

## Tool dispatch

Every tool — plain `Tool`, `HitlTool`, `BackgroundTool`, sub-agent,
MCP tool, skill view tool — winds up wrapped in an adapter that
implements the universal `ToolDispatch` trait:

```
                  ┌──────────────────┐
                  │   ToolRegistry   │   keys: tool name
                  │  HashMap<String, │   values: Arc<dyn ToolDispatch>
                  │   Arc<dyn TD>>   │
                  └────────┬─────────┘
                           │
       ┌───────────────────┼───────────────────┐
       │                   │                   │
  PlainAdapter        HitlAdapter      BackgroundAdapter
  (Tool trait)        (HitlTool trait) (BackgroundTool trait)
       │                   │                   │
       └─────── all impl ToolDispatch ─────────┘
       │
  McpTool (Tool — registered as plain via PlainAdapter)
  SubagentTool (Tool — same)
  SkillViewTool (Tool — same)
```

Adding a new tool kind = a new adapter struct (~30 lines). The registry
and dispatch path are unchanged.

## Storage abstraction

```
                    ┌───────────────────────┐
                    │  trait StorageBackend │
                    └──────────┬────────────┘
              ┌────────┬───────┴────────┬──────────────┐
              │        │                │              │
       LocalFsBackend  MemoryBackend  OverlayBackend  NamespacedBackend
       (real FS)       (BTreeMap)     (composed of N) (path-prefix routes)
```

Every consumer (skills, agents, plugins, spillover, memory layer, file
layer) takes `Arc<dyn StorageBackend>`. Swap LocalFs for an S3 backend
in one place; everything else is unaware.

## Provider abstraction

```
        ┌─────────────────────┐
        │   trait Provider    │ — complete, complete_split, complete_simple,
        └──────────┬──────────┘   model, fork, supports_image_input, ...
            ┌──────┴──────┐
            │             │
    AnthropicProvider  GeminiProvider
       (SSE)           (one-shot SSE, normalized)
```

Both providers normalize to the same `StreamEvent` enum so the agent
loop never branches on which provider produced the stream.

Retry policy lives in `runic-provider-core` and wraps the HTTP send
(not the streaming body) with classified `is_retryable` decisions.

## MCP transport abstraction

```
            ┌────────────────────┐
            │  trait Transport   │ — request, notify, close
            └──────────┬─────────┘
              ┌────────┴───────┐
              │                │
       StdioTransport   HttpTransport
       (subprocess)     (Streamable HTTP, 2025-03-26 spec)
```

`McpHandle` holds `Arc<dyn Transport>`; the rest of `runic-mcp` is
transport-agnostic. WebSocket / plugin host / etc. would be a new
`impl Transport`.

## Sub-agent dispatch

`SubagentTool` is a plain `Tool` whose `execute` builds a fresh
child `Agent` via a factory closure, runs it to completion, and
returns the child's last assistant text as the tool result.

- The child has its own `AgentState` — exploration doesn't leak into
  the parent's transcript.
- `AsyncSubagentTool` does the same but as a `BackgroundTool`: the
  call returns a task id immediately; the child runs to completion in
  a tokio task; the parent polls via `background_status`.

`runic-agents` lets you define sub-agents as `AGENT.md` files instead
of Rust code (markdown frontmatter → `MdAgent::make_subagent_tool`).

## Hooks: the six lifecycle points

```rust
trait Hook: Send + Sync {
    fn name(&self) -> &'static str;
    async fn before_agent(&self, &mut AgentState) -> HookOutcome;
    async fn after_agent(&self, &mut AgentState) -> HookOutcome;
    async fn before_model(&self, &mut AgentState) -> HookOutcome;
    async fn after_model(&self, &mut AgentState, stop_reason: Option<&str>) -> HookOutcome;
    async fn before_tool(&self, &mut AgentState, &mut ToolCall) -> HookOutcome;
    async fn after_tool(&self, &mut AgentState, &ToolCall, &ToolResult) -> HookOutcome;
}
```

Each hook returns `HookOutcome::{Continue, Stop, SubstituteToolResult}`.
`Stop` bubbles up as `AgentError::HookStop`.
`SubstituteToolResult` (in `before_tool`) skips dispatch and uses the
provided result instead — useful for caching or HITL approval.

Every hook firing is recorded as a `SessionEvent::HookRan` in
`AgentState.events`, giving you an audit trail for free.

## Runtime context (the typed bag)

`AgentBuilder::runtime(value)` stashes a typed handle that tools and
hooks retrieve via `ctx.get::<T>()`:

```rust
let agent = Agent::builder(provider)
    .runtime(DbPool::new(...))
    .runtime(MyAuthHandle::new(...))
    .tool(Arc::new(QueryTool))   // QueryTool fetches DbPool via ctx.get
    .build();
```

Keyed by `TypeId` (each type can appear at most once). The model
NEVER sees runtime context — it's purely for tool/hook implementations.

## Session events (the source of truth)

`AgentState` is an event log:

```rust
enum SessionEvent {
    RunStart      { run_id, at },
    RunEnd        { run_id, outcome, at },
    Message       { run_id, msg, at },
    TurnBoundary  { run_id, at },
    HookRan       { run_id, hook, lifecycle, note, at },
    StateSnapshot { run_id, messages, system_prompt, reason, at },
}
```

The provider message list is **derived** from this event log via
`AgentState::messages_for_provider()`. `StateSnapshot` is how compaction
rewrites history — it replaces the accumulated `Message` events with a
single curated set.

## What's not built

Listed in the order I'd build them next:

1. **Persistence** — write the `SessionEvent` log to disk via
   `StorageBackend`; resume by replaying. Foundation already exists
   (events are serializable; storage backend is abstracted).

2. **Blob / file uploads** — multipart upload + `BlobRef` content
   blocks + content-addressed storage under `blobs/`. Required for
   serve mode if you want clients to send files.

3. **Serve mode** — HTTP daemon, session identity, blob handling,
   probably WebSocket for streaming. Biggest single chunk.

4. **Slash commands** — `~/.runic/commands/{name}/COMMAND.md` files
   that the REPL intercepts in user input. Smaller than the above;
   completes the Claude-Code parity story.

5. **More reminder sources** — file watch, MCP events, provider
   errors, compaction notifications. Each one is ~50-80 lines.

## Reading the code

- Start with `crates/runic-message-types/src/lib.rs` for the wire types.
- Then `crates/runic-tool-core/src/tool.rs` for `Tool` / `ToolDispatch`.
- Then `crates/runic-agent-core/src/agent.rs::run_inner` to see the
  turn loop.
- Then `crates/runic-context-engine/src/composite.rs` and
  `reminder.rs` for the decorator pattern.
- The binary at `runic/src/main.rs` is the single place that wires
  everything together — it's where you can see the whole shape.
