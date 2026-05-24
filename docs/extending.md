# Extending runic

Four extension points cover almost everything you'd want to add:

| Extension | Trait | When to use |
|---|---|---|
| [Tool](#writing-a-tool) | `runic_tool_core::Tool` | The model needs a new capability |
| [Hook](#writing-a-hook) | `runic_agent_core::Hook` | You want to intercept the agent loop |
| [Reminder](#writing-a-reminder) | `runic_context_engine::Reminder` | The model should know about something that happened outside the conversation |
| [ContextLayer](#writing-a-contextlayer) | `runic_context_engine::ContextLayer` | You want to inject text into the system prompt |

If none of these fit, you probably want a [full ContextEngine
decorator](./context-engine.md#building-your-own-engine).

## Writing a Tool

Simplest possible tool — returns a fixed string:

```rust
use async_trait::async_trait;
use std::sync::Arc;
use runic_tool_core::{Tool, ToolContext, ToolResult};

pub struct PingTool;

#[async_trait]
impl Tool for PingTool {
    fn name(&self) -> &str { "ping" }

    fn description(&self) -> &str {
        "Returns 'pong'. Use to verify tool dispatch works."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::ok("pong")
    }
}

// Wire it up:
let agent = Agent::builder(provider)
    .tool(Arc::new(PingTool))
    .build();
```

### Reading runtime context

If your tool needs a DB pool, auth handle, etc., stash it via
`AgentBuilder::runtime` and fetch in `execute`:

```rust
#[derive(Clone)]
struct DbPool { /* ... */ }

impl Tool for QueryTool {
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let Some(db) = ctx.get::<DbPool>() else {
            return ToolResult::error("DbPool not in runtime context");
        };
        // ... use db
        ToolResult::ok("queried")
    }
    // ... name/description/schema
}

let agent = Agent::builder(provider)
    .runtime(db_pool)
    .tool(Arc::new(QueryTool))
    .build();
```

`ctx.get::<T>()` returns `Option<Arc<T>>` — None if no value of that
type was registered. Keyed by `TypeId`, so each type appears at most
once.

### HITL tools (user approval gate)

For tools that need explicit user confirmation before running:

```rust
use runic_tool_core::{HitlTool, Draft};

impl HitlTool for SendEmailTool {
    fn name(&self) -> &str { "send_email" }
    fn description(&self) -> &str { "Send an email; user reviews before sending." }
    fn input_schema(&self) -> serde_json::Value { /* ... */ }

    fn draft(&self, input: &serde_json::Value) -> Draft {
        Draft {
            summary: format!("Send email to {}", input["to"]),
            current_input: input.clone(),
            input_schema: self.input_schema(),
            editable_fields: vec!["to".into(), "subject".into(), "body".into()],
        }
    }

    async fn execute(&self, final_input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        // Called only AFTER the user approves (and possibly edits) the draft
        ToolResult::ok("sent")
    }
}

let agent = Agent::builder(provider)
    .runtime::<ApproverHandle>(my_approver_handle)   // implements Approver trait
    .hitl_tool(Arc::new(SendEmailTool))
    .build();
```

`Approver` trait is implemented by your UI layer (REPL prompt, web
form, IDE dialog). It receives the `Draft`, returns `UserDecision`.

### Background tools (long-running work)

For tools that should return a task id immediately and keep working:

```rust
use runic_tool_core::BackgroundTool;

#[async_trait]
impl BackgroundTool for FetchEverythingTool {
    fn name(&self) -> &str { "fetch_everything" }
    fn description(&self) -> &str { "Fetches a large dataset in the background." }
    fn input_schema(&self) -> serde_json::Value { /* ... */ }

    async fn run(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        // This runs in a tokio task; can take minutes
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        ToolResult::ok("fetched all the things")
    }
}

let agent = Agent::builder(provider)
    .background_tool(Arc::new(FetchEverythingTool))
    .build();
```

The model gets back a `task_id` immediately and polls via the
auto-registered `background_status` tool (or just sees a
`BackgroundTaskReminder` note when it completes).

## Writing a Hook

Hooks fire at six points in the agent loop. Each method has a default
no-op impl; override only what you care about.

```rust
use async_trait::async_trait;
use runic_agent_core::{Hook, HookOutcome, AgentState};
use runic_message_types::ToolCall;
use runic_tool_core::ToolResult;

pub struct LoggingHook;

#[async_trait]
impl Hook for LoggingHook {
    fn name(&self) -> &'static str { "logging" }

    async fn before_tool(
        &self,
        _state: &mut AgentState,
        call: &mut ToolCall,
    ) -> HookOutcome {
        eprintln!("[hook] dispatching tool: {}", call.name);
        HookOutcome::Continue
    }

    async fn after_tool(
        &self,
        _state: &mut AgentState,
        call: &ToolCall,
        result: &ToolResult,
    ) -> HookOutcome {
        eprintln!("[hook] {} finished, is_error={}", call.name, result.is_error);
        HookOutcome::Continue
    }
}

let agent = Agent::builder(provider)
    .hook(Arc::new(LoggingHook))
    .build();
```

`HookOutcome` variants:
- `Continue` — proceed normally
- `Stop` — abort the run; bubbles up as `AgentError::HookStop`
- `SubstituteToolResult(result)` — (in `before_tool` only) skip the
  tool dispatch and use this synthetic result instead

Use `SubstituteToolResult` for things like caching, rate-limit
blocking, or HITL flows handled outside the regular `HitlTool` machinery.

### Mutating tool calls

`before_tool` receives `&mut ToolCall` — you can rewrite the call's
`name` or `input` before dispatch:

```rust
async fn before_tool(&self, _state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
    if call.name == "read_file" {
        // Force all reads through a sandboxed variant
        call.name = "sandboxed_read_file".into();
    }
    HookOutcome::Continue
}
```

## Writing a Reminder

A `Reminder` is a pluggable source of ambient context. Each turn the
`ReminderEngine` calls every registered reminder and folds the notes
into the prompt.

Example: notify the model when a file changes externally.

```rust
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use runic_context_engine::{AmbientNote, Reminder, TurnContext};
use runic_storage_backend::StorageBackend;

#[derive(Debug)]
pub struct FileChangeReminder {
    storage: Arc<dyn StorageBackend>,
    watched: Vec<String>,
    // last-seen modification time per watched file
    last_seen: Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>,
}

impl FileChangeReminder {
    pub fn new(storage: Arc<dyn StorageBackend>, paths: Vec<String>) -> Self {
        Self {
            storage,
            watched: paths,
            last_seen: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Reminder for FileChangeReminder {
    fn name(&self) -> &str { "file-changes" }

    async fn collect(&self, _ctx: &TurnContext<'_>) -> Vec<AmbientNote> {
        let mut notes = Vec::new();
        let mut last_seen = self.last_seen.lock().await;

        for path in &self.watched {
            let Ok(meta) = self.storage.metadata(path).await else { continue };
            let Some(modified) = meta.modified else { continue };

            let changed = match last_seen.get(path) {
                Some(prev) => modified > *prev,
                None => true,  // first time seeing it — don't announce
            };

            if changed && last_seen.contains_key(path) {
                notes.push(AmbientNote {
                    source: format!("file-watch:{path}"),
                    content: format!("file '{path}' was modified externally at {modified}"),
                    dedup_key: Some(format!("file-change:{path}:{}", modified.timestamp())),
                });
            }
            last_seen.insert(path.clone(), modified);
        }
        notes
    }
}

// Wire it up:
let engine = ReminderEngine::new(inner)
    .with_reminder(BackgroundTaskReminder::new(bg_manager))
    .with_reminder(FileChangeReminder::new(storage, vec!["memory/MEMORY.md".into()]));
```

Tips:
- Use `dedup_key` to make the same event fire at most once. Include
  the timestamp so a re-modification re-announces.
- Keep `collect()` fast — it runs on every turn.
- It's fine to return an empty vec; the engine just skips it.

## Writing a ContextLayer

A `ContextLayer` contributes text to the system prompt. Each layer
gets called by `CompositeEngine`, its output joined with `\n\n`.

Example: inject the current time of day.

```rust
use async_trait::async_trait;
use runic_context_engine::{ContextLayer, TurnContext};

pub struct TimeOfDayLayer;

#[async_trait]
impl ContextLayer for TimeOfDayLayer {
    fn name(&self) -> &str { "time-of-day" }

    async fn render(&self, _ctx: &TurnContext<'_>) -> Option<String> {
        let now = chrono::Local::now();
        Some(format!(
            "<current-time>\n{} ({})\n</current-time>",
            now.to_rfc3339(),
            now.format("%A, %B %-d")
        ))
    }
}

// Wire it up:
let engine = CompositeEngine::new()
    .with_layer(BasePromptLayer::new("You are a helpful assistant."))
    .with_layer(TimeOfDayLayer)
    .with_layer(PersonaLayer::new(storage.clone(), "SOUL.md"));
```

The layer is called on every turn — perfect for stable-but-updated
context like time, the latest commit hash, branch name, etc.

Return `None` to contribute nothing (e.g. when there's nothing useful
to render). The composite engine drops `None` results before joining.

## When to write what

| Goal | Best fit |
|---|---|
| New capability the model invokes | Tool |
| New capability the user must approve | HitlTool |
| New long-running operation | BackgroundTool |
| Observe / log / mutate agent behavior | Hook |
| Notice changes outside the conversation, surface to model | Reminder |
| Add a static section to the system prompt | ContextLayer |
| Add complex pipeline behavior (rewrite messages, etc.) | Full ContextEngine decorator |

## Recommended reading

- `crates/runic-tool-core/src/tool.rs` — `Tool`, `ToolDispatch`,
  `ToolRegistry`, all the adapters
- `crates/runic-agent-core/src/hooks.rs` — `Hook` trait + tests
- `crates/runic-context-engine/src/reminder.rs` —
  `BackgroundTaskReminder` is the simplest reference impl
- `crates/runic-context-engine/src/layers/` — five built-in layer impls
- `crates/runic-examples/` — runnable examples of each
