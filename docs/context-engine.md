# Context Engine

The **context engine** owns the lifecycle of the prompt: system-prompt
assembly, ambient context injection, tool-result mediation, and
compaction. The agent holds ONE `Arc<dyn ContextEngine>`, but in
practice that engine is a stack of decorators.

## The trait

```rust
#[async_trait]
pub trait ContextEngine: Send + Sync {
    async fn assemble_system_prompt(&self, ctx: &TurnContext<'_>) -> String { ... }
    async fn ambient_notes(&self, _ctx: &TurnContext<'_>) -> Vec<AmbientNote> { Vec::new() }
    async fn process_user_input(&self, _ctx: &TurnContext<'_>, msg: Message) -> Message { msg }
    async fn maybe_compact(&self, _ctx: &TurnContext<'_>, _messages: &mut Vec<Message>) {}
}
```

Four method points where the engine can act:

| Method | When it fires | What it does |
|---|---|---|
| `assemble_system_prompt` | Once per turn, right before sending to the provider | Builds the stable system prompt |
| `ambient_notes` | Once per turn | Returns volatile context (injected as `system_dynamic` after the latest user message — doesn't perturb the cached system prompt prefix) |
| `process_user_input` | Once per run, when the user types | Can rewrite the user's message (redact secrets, spill huge pastes, etc.) |
| `maybe_compact` | Once per turn, right before sending | Can mutate the messages list in place (rewrite, drop, summarize) |

All have default impls. Implement only the points you care about.

## The standard chain

The binary wires four engines in this order:

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

Each engine delegates the methods it doesn't own to the inner engine.
Adding a behavior = a new decorator. Removing one = unwrap it in
`main.rs`.

## CompositeEngine + layers

`CompositeEngine` assembles the system prompt by concatenating the
output of every `ContextLayer` it holds, in registration order.

```rust
use runic_context_engine::{
    CompositeEngine, BasePromptLayer, PersonaLayer, UserFactsLayer,
    MemoryLayer,
};

let engine = CompositeEngine::new()
    .with_layer(BasePromptLayer::new("You are a focused assistant."))
    .with_layer(PersonaLayer::new(storage.clone(), "SOUL.md"))
    .with_layer(UserFactsLayer::new(storage.clone(), "memory/USER.md"))
    .with_layer(MemoryLayer::new(storage.clone(), "memory/MEMORY.md"));
```

Built-in layers:

| Layer | Reads | Wraps content in |
|---|---|---|
| `BasePromptLayer::new(text)` | the literal `text` | nothing |
| `PersonaLayer::new(storage, path)` | `SOUL.md` (or wherever) | `<persona>...</persona>` |
| `UserFactsLayer::new(storage, path)` | `memory/USER.md` | `<user-facts>...</user-facts>` |
| `MemoryLayer::new(storage, path)` | `memory/MEMORY.md` | `<memory>...</memory>` |
| `FileLayer::new(storage, path)` | arbitrary path | nothing (raw passthrough) |

Each layer has a configurable preamble explaining what the block is.
Override via `.with_preamble("...")` or `.with_preamble("")` to
suppress.

Custom layers: see [extending.md](./extending.md#writing-a-contextlayer).

## CompactorEngine

When total estimated tokens (chars / 4 heuristic) exceed
`token_threshold`, summarizes the oldest messages via a `Provider`
call, replaces them with a single synthetic message.

```rust
use runic_context_engine::CompactorEngine;

let compacted = CompactorEngine::new(inner, provider.clone())
    .with_token_threshold(100_000)      // default
    .with_keep_recent(8);               // always keep the latest N verbatim
```

Defaults:
- `token_threshold`: 100,000 tokens (~400K chars)
- `keep_recent`: 8 messages

Failure mode: if the summarization call errors, the original messages
are restored. Never silently loses history.

Configurable via env: `RUNIC_COMPACT_THRESHOLD=50000 cargo run`.

## SpilloverEngine

When a tool result exceeds `threshold_bytes`, writes it to
`spillover/{run_id}/{tool_use_id}.txt` via the storage backend.
Replaces the in-context content with a preview + path.

```rust
use runic_context_engine::SpilloverEngine;

let spilled = SpilloverEngine::with_settings(
    inner,
    storage.clone(),
    "spillover",     // root prefix on storage
    8 * 1024,        // threshold_bytes
    800,             // preview_chars
);
```

Defaults: 8 KiB threshold, 800-char preview.

Spilled tool results are tracked in-memory by `tool_use_id` — once
replaced, the SAME replacement text is used across turns, so the
rendered prompt is byte-stable (prompt-cache friendly).

Configurable via env: `RUNIC_SPILLOVER_THRESHOLD=4096 cargo run`.

## ReminderEngine

Surfaces things that happened **outside the conversation** as ambient
notes the model sees in its next turn. Decorator engine that holds a
list of pluggable `Reminder` sources.

```rust
use runic_context_engine::{ReminderEngine, BackgroundTaskReminder};

let with_reminders = ReminderEngine::new(inner)
    .with_reminder(BackgroundTaskReminder::new(background_manager.clone()));
```

`BackgroundTaskReminder` watches a `BackgroundManager` for completed /
errored / cancelled tasks and announces each one exactly once. Use the
`dedup_key` on `AmbientNote` to enforce once-only delivery.

Writing custom reminders: see [extending.md](./extending.md#writing-a-reminder).

## TurnContext

Every method on `ContextEngine` gets a `TurnContext`:

```rust
pub struct TurnContext<'a> {
    pub base_system_prompt: &'a str,
    pub messages: &'a [Message],
    pub run_id: &'a str,
    pub turn: u32,
    pub config: &'a serde_json::Map<String, serde_json::Value>,
}
```

- `base_system_prompt`: the literal prompt the user set on
  `AgentBuilder::system_prompt(...)`. Layers can use it or ignore it.
- `messages`: the messages that WILL be sent to the provider this
  turn. Layers can scan them for context-aware rendering.
- `run_id`: stable for the duration of one `Agent::run` call.
- `turn`: monotonically increasing within a run.
- `config`: the **per-run config map** for this run (see below). Empty
  on runs started without a `RunContext`.

`maybe_compact` ALSO gets `&mut Vec<Message>` (the actual list, not a
snapshot) so it can mutate.

### Per-run config

`config` is the open map set by [`RunContext`](./extending.md#per-run-context-runcontext)
for the current run — request-scoped values like `user_id`,
`allow_web_search`, a tenant or locale. It lets a layer personalize the
prompt without a typed schema, and without baking request data into the
pooled agent. It's overwritten every run, so it never leaks.

```rust
async fn render(&self, ctx: &TurnContext<'_>) -> Option<String> {
    // Render this layer only for runs that carry a user_id.
    let user = ctx.config.get("user_id").and_then(|v| v.as_str())?;
    Some(format!("You are assisting user `{user}`."))
}
```

The same map is reachable from tools (`ctx.config(key)`) and hooks
(`state.config(key)`). Runnable example: `cargo run --example
with_run_context`.

## AmbientNote

```rust
pub struct AmbientNote {
    pub source: String,
    pub content: String,
    pub dedup_key: Option<String>,
}
```

Notes returned from `ambient_notes` get joined with `\n\n` and sent
to the provider as `system_dynamic` (via `Provider::complete_split`).
This means the stable system prompt prefix stays cache-warm even when
ambient notes change.

`dedup_key`:
- `None` → re-emitted every turn (use for perpetual "current state" notes)
- `Some(key)` → emitted at most once per `ReminderEngine` lifetime

## Building your own engine

The decorator pattern is the recommended way:

```rust
use async_trait::async_trait;
use std::sync::Arc;
use runic_context_engine::{ContextEngine, TurnContext, AmbientNote};
use runic_message_types::Message;

pub struct MyEngine {
    inner: Arc<dyn ContextEngine>,
    // your fields here
}

#[async_trait]
impl ContextEngine for MyEngine {
    async fn assemble_system_prompt(&self, ctx: &TurnContext<'_>) -> String {
        // Usually delegate. Override only if you have a reason.
        self.inner.assemble_system_prompt(ctx).await
    }

    async fn ambient_notes(&self, ctx: &TurnContext<'_>) -> Vec<AmbientNote> {
        let mut notes = self.inner.ambient_notes(ctx).await;
        // Add your contributions
        notes.push(AmbientNote {
            source: "my-engine".into(),
            content: "something noteworthy".into(),
            dedup_key: Some("once".into()),
        });
        notes
    }

    async fn process_user_input(&self, ctx: &TurnContext<'_>, msg: Message) -> Message {
        self.inner.process_user_input(ctx, msg).await
    }

    async fn maybe_compact(&self, ctx: &TurnContext<'_>, messages: &mut Vec<Message>) {
        self.inner.maybe_compact(ctx, messages).await;
        // Do your work after the inner ran
    }
}
```

Wire it into the binary:

```rust
let inner: Arc<dyn ContextEngine> = Arc::new(composite);
let wrapped: Arc<dyn ContextEngine> = Arc::new(MyEngine { inner, /* ... */ });

agent_builder.context_engine_arc(wrapped)
```

`context_engine_arc` takes `Arc<dyn ContextEngine>` directly (the
type-erased shape decorator chains produce). The simpler
`.context_engine(MyEngine { ... })` form takes `impl ContextEngine`
by value — easier when you have a concrete type, harder when you have
an Arc.

## Order matters

Decorator engines compose, so the order you wrap them in determines
WHO runs WHEN:

```rust
// Compaction runs INSIDE spillover's maybe_compact wrapper:
let compacted: Arc<dyn ContextEngine> = Arc::new(CompactorEngine::new(composite, provider));
let spilled:   Arc<dyn ContextEngine> = Arc::new(SpilloverEngine::new(compacted, storage));
// Spillover sees post-compaction messages; spills any huge surviving results.
```

If you flipped the order, spillover would replace large tool results
with previews FIRST, and compaction would summarize the (now smaller)
messages — both correct but with different semantics. The standard
order (compact → spill) reflects: compaction shrinks history; spillover
shrinks individual blocks.

`ReminderEngine` is naturally outermost because it only contributes to
`ambient_notes` and forwards everything else.

## Testing

`crates/runic-context-engine` ships 60 tests covering composite
ordering, decorator delegation (inner methods get called),
spillover cache behavior across turns, compactor threshold + restore
on failure, and reminder dedup + per-source isolation.

Look at `crates/runic-context-engine/src/reminder.rs` tests — they
include a `FixedReminder` test double that you can copy for your own
custom reminder tests.
