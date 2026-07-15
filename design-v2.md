# State v2 — design

## Goal

The log must be able to draw itself. From the stored events alone:
`run → turns (tokens, model latency) → tool calls (duration, error) → delegation edges (child usage, duration)`,
plus how every run ended. One derived view, `timeline()`, renders that tree.

The live stream (`AgentEvent`) already has `ToolStarted`/`ToolFinished`/`TurnCompleted` —
the loop already knows every moment we need; it just never persists them and doesn't
time them. This design is mostly "promote what the live stream knows into the durable
log, with timing and usage attached."

## 1. Tool results: one outcome enum, JSON output, grounding, retention

Today's `ToolResult` is three different outcomes pretending to be fields
(`success`/`error` flags, `persisted_output` as a bolt-on, `deferred` as a
not-a-result), and nothing prevents nonsense combinations. v2 is an enum:

```rust
pub enum ToolResult {
    Done {
        output: serde_json::Value,                    // was String
        grounding: Option<Vec<GroundingSource>>,
        retention: Retention,
    },
    Failed { message: String },
    Deferred { channel: String, payload: serde_json::Value },
}
```

Constructors keep call sites tiny and mostly source-compatible:
`ToolResult::ok(impl Into<Value>)` (Done, no grounding, Full retention —
`&str`/`String` → `Value::String` so existing tools keep compiling),
`.with_grounding(sources)`, `.with_summary(s)`, `.spill()`,
`ToolResult::error(msg)`, `ToolResult::defer(channel, payload)`.

### JSON output

- `ContentBlock::ToolResult.content` also becomes `Value`. Old persisted logs still
  deserialize (a JSON string is a `Value`).
- Provider encoding rule, one place per driver:
  - **Gemini** passes the `Value` natively into `FunctionResponse.response`
    (finally correct — today we wrap a string).
  - **Anthropic / OpenAI / Mistral** pass `Value::String` as-is and `to_string()`
    anything else.
- Payoffs: `McpTool` and Composio stop flattening natively-JSON results; hooks can
  inspect result *fields* instead of parsing strings; wire previews and UIs can render
  structure; a future "delegate returns structured child output" is free.

### Retention — replaces `persisted_output`

```rust
pub enum Retention {
    Full,                 // default — output persists in history as-is
    Summary(Value),       // today's trick: history keeps the summary,
                          // model sees the full output exactly once
    Artifact,             // output spilled to the ArtifactStore; history keeps
                          // an ArtifactRef block + short preview
}
```

The flaw in today's mechanism: the full output lives only in a dispatch-memory
overlay for one model call, then evaporates — model, UI, and audit can never see
it again without re-running the tool, and it doesn't survive a restart.

`Retention::Artifact` fixes that with infrastructure we already have
(`ArtifactStore`, `ContentBlock::ArtifactRef`, `ArtifactResolver`): the model
still gets the full payload on the immediate next call, but afterwards the output
is durable and re-fetchable — the UI can open it, and the model can have it
re-inlined later through the existing media-resolver path. Wiring: dispatch needs
the artifact store handle (the composer already holds it for the resolver — make
it explicit on `AgentBuilder`).

Optional builder policy: `auto_spill_over(bytes)` — outputs above the threshold
from tools that never set a retention get spilled automatically instead of
flooding history.

### Grounding — provenance as a first-class channel

```rust
pub struct GroundingSource {
    pub source: String,              // URL, artifact id, file path, memory key…
    pub title: Option<String>,
    pub snippet: Option<String>,
    pub metadata: Option<Value>,
}
```

The sources that *back* the output, separate from the output: `web_search` returns
the answer as `output` and URLs+snippets as grounding; a retrieval tool returns
chunks + document ids; weather grounds on the API endpoint it hit.

Rules:
- **Persisted** in the log (optional field on the `ToolResult` content block) and
  surfaced on the wire (`ToolFinish` gains it) for citation rendering.
- **Never sent to the model** — providers skip it when encoding. A tool that wants
  the model to see sources puts them in `output` deliberately. Grounding is an
  audit/UI channel, not invisible prompt bloat.

## 2. New/changed `SessionEvent` variants

Discipline: **payloads stay in `Message` blocks (ToolUse args, ToolResult content) —
the new events carry only identity, timing, and status.** No double-storage.

```rust
RunStart      { run_id, agent, config: Option<Map>, at }
// + sanitized run-config stamp (audit: "as which user_id did this run act")

RunError      { run_id, error: String, at }
// NEW — terminal; the log becomes self-sufficient about failures

TurnEnd       { run_id, turn: u32, usage: TokenUsage, model_ms: u64, at }
// REPLACES TurnBoundary

ToolStarted   { run_id, turn: u32, call_id, tool, at }              // NEW
ToolFinished  { run_id, call_id, tool, is_error, duration_ms, at }  // NEW

DelegationStarted  { run_id, call_id, agent, mode, child_session: Option<String>, at }
// NEW — sync + parallel (background keeps TaskSpawned/Finished, which gains the
// same optional child_session field). child_session set when a ChildPersistence
// handle was present (§7).

DelegationFinished { run_id, call_id, agent, status, usage: TokenUsage, duration_ms, at }
// child usage from its RunOutcome — carried on the edge even when the child
// transcript isn't persisted
```

`RunEnd`, `Message`, `HookFired`, `StateSnapshot`, `StateUpdated`, `ToolDeferred`,
`TaskSpawned/Finished` unchanged.

No `TurnStart` — a turn starts at the previous `TurnEnd`/`RunStart` timestamp, so it
would be pure noise.

## 3. `ThreadStats` v2

Boundary rule: **ThreadStats answers "how much, lifetime" (counters) and "where am I
now" (gauges) in O(1); `timeline()` and the run rows answer "what happened, when."**
No distributions, no per-run detail, no time windows — that boundary is what stops
the struct from growing forever.

```rust
pub struct ThreadStats {
    // runs — attempts and outcomes, not just completions
    pub runs: u64,             // ← RunStart (attempts; "every 5 interactions" hooks
                               //   count failures too, as they should)
    pub errored_runs: u64,     // ← RunError
    pub cancelled_runs: u64,   // ← RunEnd with stop_reason "cancelled"

    // the thread's OWN model activity — ← TurnEnd
    pub turns: u64,
    pub input_tokens: u64,     // lifetime cumulative — billing, NOT compaction
    pub output_tokens: u64,
    pub model_ms: u64,

    // gauge, not counter — ← overwritten on every TurnEnd
    pub context_tokens: u64,   // input tokens of the LAST model call = the actual
                               // current context size, as billed by the provider.
                               // THE compaction trigger; self-corrects after a
                               // compaction snapshot shrinks the context. Replaces
                               // the chars/4 guess in CompactionHook.

    // tools — ← ToolFinished
    pub total_tool_calls: u64,
    pub tools: HashMap<String, ToolStat>,

    // children — ← DelegationFinished
    pub delegations: u64,
    pub delegation_errors: u64,
    pub delegated_usage: TokenUsage,

    // background tasks — ← TaskSpawned / TaskFinished (unchanged)
    pub tasks_spawned: u64,
    pub tasks_finished: u64,
    pub tasks_failed: u64,
}

pub struct ToolStat {
    pub calls: u64,
    pub errors: u64,
    pub total_duration_ms: u64,   // mean = total / calls
    pub max_duration_ms: u64,     // outlier detection: "which tool spiked"
}
```

Decisions:

- **`runs` counts attempts (← `RunStart`), outcomes separately.** Today `runs` folds
  from `RunEnd`, so an errored run isn't a run at all — wrong, and it makes
  "every N interactions" hooks drift under failures. Successful =
  `runs − errored − cancelled`, derivable.
- **Counters vs gauges.** Lifetime sums answer billing; `context_tokens` answers
  "how full is the context right now". Triggering compaction on the lifetime sum is
  a bug (it never shrinks — you'd compact forever after the first threshold cross).
- **Own tokens vs delegated tokens strictly separate.** `input/output_tokens` fold
  only from this thread's `TurnEnd`s; child consumption arrives via
  `DelegationFinished.usage` into `delegated_usage`. "Thread total cost" =
  own + delegated; "this agent's model burn" = own. Merged, both questions become
  unanswerable.
- **Tool counting moves off message-block digging** onto `ToolFinished` — simpler,
  gains errors + latency.
- **Snapshot-authoritative stays** exactly as-is (`StateSnapshot.stats` replaces
  wholesale).

Discussed, pending a call:

1. **Cache tokens in `TokenUsage`** (`cache_read/cache_write`) — real cost needs
   them (cached input ~10x cheaper); a runic-types change rippling through all four
   providers. Cheap now, painful after logs accumulate. Recommended: yes, in v2.
2. **Per-model breakdown** (`tokens_by_model: HashMap<String, TokenUsage>`) — needed
   the moment a thread delegates to a different model; requires `TurnEnd` /
   `DelegationFinished` to carry the model name. Recommended: yes, in v2.
3. **Wasted-work counters** (`guard_trips`, `hook_cancels`, `compactions`) — makes
   degenerate threads visible in a dashboard list without opening the timeline.
4. **Stats on the `done` wire event** — dashboards update live without a refetch.
5. **Time windows / tenant rollups are NOT stats** — they're store-side queries over
   the timestamped run rows (roadmap: `SessionStore::usage(tenant, from, to)` for
   per-tenant billing).

## 4. `timeline()` — the diagram

```rust
AgentState::timeline() -> Vec<RunTrace>

RunTrace  { id, agent, status: Ok|Error(String)|InFlight, started_at, ended_at, usage,
            turns: Vec<TurnTrace> }
TurnTrace { turn, usage, model_ms, tools: Vec<ToolTrace>,
            delegations: Vec<DelegationTrace> }
ToolTrace { call_id, tool, duration_ms, is_error }
DelegationTrace { call_id, agent, status, usage, duration_ms }
```

Pure derived fold — no stored structure, no span subsystem. On a live `AgentState` it
covers the working set (post-snapshot); a serve endpoint
(`GET /threads/{id}/timeline`) folds the **full stored log** for complete history.
This is the exact answer to "give me a diagram of the state."

## 5. Thread metadata

`SessionMeta` today: `{ session_id, label, event_count, created_at, last_activity }`.
Add two columns: **`agent`** and **`parent_session: Option<String>`** (§7 — child
sessions point back at their parent; thread-list endpoints filter
`parent IS NULL` by default so UIs don't drown in subagent sessions, with an
explicit way to list a thread's children). Everything else you'd want per-thread
(run count, last run status, total usage) already lives in the run rows and stats —
the thread-list endpoint should *join* it, not store it twice. Fixing metadata is an
aggregation problem in serve, not a schema problem in substrate.

## 6. Emission points (all already exist)

- Turn loop → `TurnEnd` (usage from the provider response it already holds,
  `model_ms` from timing the provider call).
- Dispatch → `ToolStarted`/`ToolFinished` (it already emits the live `AgentEvent`
  twins — same spot, add an `Instant` between them).
- Run error path → `RunError` before returning.
- `DelegateTool` → delegation events around `run_child` (child usage from the
  `RunOutcome` it already gets back).
- Replay/wire: the stored log can now replay `ToolStart`/`ToolFinish`/`TurnComplete`
  wire events too — replay fidelity improves for free.

## 7. Subagent execution persistence

Child runs become real, inspectable sessions. The hard problem — where a child's
persist sink comes from at delegate time (`runic-subagent` must not know
`SessionStore`) — is solved with one abstract handle.

### The seam

```rust
// in runic-state — no substrate dependency
pub trait ChildPersistence: Send + Sync {
    fn begin(&self, child_session: &str, agent: &str) -> ChildRun;
    // ChildRun: a PersistSink plus a way to await its drain
}
```

- Serve implements it over the `SessionStore` (same persister it uses for parents)
  and injects it into the parent's `RunContext` in `build_run_context`.
- `DelegateTool` picks it up via `ctx.get::<Arc<dyn ChildPersistence>>()` — the same
  mechanism it already uses for `ExternalEvents` — threads it through
  `DelegationCtx`, and wires the child agent's state before running it.
- **Handle absent → today's ephemeral behavior.** Embedded users opt in by inserting
  their own impl. Nothing breaks; layering stays clean.

### Child sessions

- Session id: `{parent_session}:{call_id}` — deterministic, collision-free (call ids
  are unique per parent).
- Both directions navigable: parent's `DelegationStarted.child_session` points down;
  the child's `SessionMeta.parent_session` points up (§5).
- Identity fix that falls out (a real tenant-isolation bug today):
  `SubagentBuilder::identity()` defaults to the fake tenant `("subagent", def.name)`
  — a child tool touching anything tenant-scoped at runtime reads the wrong
  tenant's data. `DelegationCtx` gains the parent's `tenant` + `session`, and the
  default identity becomes `(parent_tenant, derived_child_session)`. `identity()`
  stays overridable.

### Durability discipline

Same rule as serve's durable-before-observable: the child's sink is **flushed
before** the parent emits `DelegationFinished` — nobody observes a completed
delegation whose transcript isn't durable yet. Child persist *failures* don't fail
the delegation (warn + degrade), but the ordering is strict.

### Depth

Grandchildren just work: the handle rides `DelegationCtx` → re-inserted into the
child's `RunContext` → the child's own delegate tool finds it exactly like the
parent did. Each level gets its own session with its own back-pointer; the whole
tree is walkable from the root thread. Depth limits already apply.

## 8. Deliberately OUT (phase 2)

- **Live child streaming** into the parent's SSE (the parent's broadcast channel has
  no origin tagging). This is the deferred stream-detail-flag idea — Lean/Standard/
  Full verbosity on `RunContext`, where Full includes child deltas. Until then the
  delegation events cover "subagent is working…" live, and the full child transcript
  is one fetch away after the fact.
- State namespacing (ADK's `app:/user:` prefixes) — our answer stays "cross-session
  state is runic-memory's job."

## 9. Migration stance

Beta, break clean: `TurnBoundary` is removed, old dev logs get wiped/re-cut. No
aliases, no shims. The one serde nicety we keep: old `ToolResult` string content
deserializes naturally as `Value::String`.

## 10. Test plan

- Extend the fold-determinism proptest to the new variants.
- Update `event_order.rs` expectations.
- One golden `timeline()` test (scripted run: 2 turns, tool call, sync delegation →
  exact tree with durations).
- Stats latency/error assertions.
- A wire-replay test for the promoted events.
- Retention: `Summary` keeps today's overlay semantics (full output exactly once);
  `Artifact` round-trips — spill, ArtifactRef in history, re-inline via resolver;
  auto-spill threshold triggers only when the tool set no retention.
- Grounding: persisted on the block, present on the wire, absent from every
  provider request (per-driver encoding tests).
- Child persistence: with the handle present, a delegation produces a child session
  whose log replays to the child's exact transcript, linked both ways
  (`DelegationStarted.child_session` ↔ `SessionMeta.parent_session`); child flushed
  before `DelegationFinished` lands; handle absent → no child session, everything
  else unchanged; nested delegation produces a walkable three-level tree; child
  sessions excluded from the default thread list.

## Open calls (overridable)

1. Tokens attribute from `TurnEnd` (per-turn truth) rather than `RunEnd`.
2. Background delegations keep their `Task` events instead of also emitting delegation
   events — the timeline merges both, no double-logging.
