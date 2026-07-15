# State v2 - design critique

## Verdict

The central direction is correct: persist execution facts once, then derive stats,
run traces, timelines, and UI views from that durable log. Promoting facts already
known by the agent loop into `SessionEvent` is a better foundation than adding a
second tracing model inside state.

The current proposal is not implementation-ready, however. It combines four large
changes whose failure semantics and storage invariants are not yet fully defined:

1. Durable execution events and terminal run state.
2. Structured tool output, grounding, and retention.
3. Lifetime statistics and timeline projections.
4. Persistent child sessions and thread hierarchy.

The issues below should be resolved in the design before implementation starts.

## P0 findings

### 1. Fix child tenant identity independently of State v2

The design correctly identifies a real isolation problem: the default subagent
identity is currently `("subagent", def.name)`. A child tool that uses the agent
identity to access tenant-scoped state can therefore operate under the wrong
tenant.

This fix must not depend on child persistence. Ephemeral children also need the
correct tenant identity. The default child identity should inherit the parent
tenant immediately, whether or not a `ChildPersistence` implementation is
installed.

Relevant locations:

- `design-v2.md`, section 7, "Child sessions"
- `crates/runic-subagent/src/delegate.rs`, `SubagentBuilder::identity`

### 2. Do not persist the open run-config map

`RunStart { config: Option<Map> }` is unsafe without a strict contract. The open
run config may contain credentials, authorization context, private user data,
provider configuration, or application-specific secrets. The word "sanitized"
does not define an enforceable security boundary.

Replace the arbitrary map with a typed audit stamp or an explicit allowlist. The
default must persist no application config values. Safe candidates include stable
identifiers such as agent name, model name, provider name, and an application-
supplied actor identifier. The application should control whether actor values are
stored directly, hashed, or omitted.

Required rules:

- Never clone `RunContext.config` into the log.
- Define the exact fields and maximum sizes.
- Define redaction before serialization, not after persistence.
- Test that unknown config keys and secret-like values never reach `RunStart`.

## P1 findings

### 3. Define an exactly-once terminal-event invariant

The proposal introduces `RunError` as a terminal event while retaining `RunEnd`,
but it does not define their relationship. A durable timeline needs this invariant:

> Every persisted `RunStart` is followed by at most one terminal event. A normally
> finished, cancelled, or suspended run and a failed run have unambiguous terminal
> semantics. A process crash may leave a run in flight.

The current loop has hook calls using `?` both before the main loop and while
finishing a successful run. Those errors can escape outside the existing error
branch and leave a started run without a terminal event. Adding `RunError` only to
the current `Err` branch will not solve this.

The design must specify:

- Whether failure emits only `RunError` or a typed `RunEnd`.
- Whether suspension is terminal or a durable non-terminal checkpoint.
- Whether resume continues the same run ID or begins a new attempt.
- How cancellation differs from failure.
- How duplicate terminal emission is prevented.
- Which outer boundary owns finalization for hook, provider, dispatch, and model
  preparation failures.

A single typed terminal event may be simpler than two variants, but either design
is acceptable if the invariant is explicit and enforced by one finalizer.

### 4. Tool timing cannot be added at the current live-event positions

The current dispatcher emits all `ToolStarted` events before it executes any tool.
For a serial batch, later tools would therefore include the execution and queue
time of earlier tools. That is not tool duration.

Timing should begin immediately around each individual `dispatch_one` execution.
The durable start event and monotonic timer should be created at that boundary.
Parallel and serial dispatch must use the same instrumentation path.

The current live events also omit hook-substituted calls. The design must decide
whether stats count:

- Model-requested tool calls.
- Actual tool executions.
- Hook substitutions.
- Guard blocks.
- Unknown tools.

One counter cannot answer all of those questions. At minimum, a tool status enum
should replace `is_error`. Useful statuses include success, tool error, execution
error, timeout, cancelled, panic, unknown tool, guard-blocked, and substituted.

`ToolFinished` should repeat `turn` rather than depending on an earlier start event
for placement. This keeps the event locally useful when logs are truncated,
compacted, partially recovered, or inspected from a cursor.

### 5. Artifact-backed tool results do not fit the current content model

The proposal says history will keep an `ArtifactRef` block instead of the tool
output. A standalone `ArtifactRef` cannot replace a `ToolResult`, because provider
protocols still require the result to be associated with the original
`tool_use_id`.

The existing artifact resolver also does not provide the behavior described in the
proposal. It expands references only on the latest user message. Older references
become a reminder to call `read_thread_artifact`; they are not automatically
re-inlined. It also does not resolve an artifact nested inside a tool result.

The durable tool-result block needs an output payload that preserves tool-call
identity while representing either inline or artifact-backed content. Request
preparation can then project that payload into each provider's required format.

The design must also define:

- The serialized bytes stored for a JSON value.
- MIME type and filename policy.
- Whether the immediate next model call still receives the full result.
- What happens when artifact storage fails.
- What happens when artifact storage succeeds but event persistence fails.
- Orphan detection and garbage collection.
- Tenant and session ownership checks.
- Maximum inline and artifact-backed output sizes.
- Whether `auto_spill_over` measures serialized UTF-8 bytes or in-memory size.

Automatic spilling should apply only after deterministic serialization and should
never silently turn a failed artifact write into an unbounded event-log write.

### 6. `serde_json::Value` requires provider-specific projection

Using `serde_json::Value` as the internal tool-output representation is a good
decision. Passing it natively to every provider is not.

Gemini's `FunctionResponse.response` is an object/Struct. An arbitrary JSON string,
number, array, boolean, or null cannot always be placed there directly. Non-object
values need a stable envelope such as `{ "result": value }`.

OpenAI function-call output supports a JSON string or supported content parts.
Mistral's documented function-calling examples serialize structured results to a
JSON string. Anthropic tool results similarly require provider content encoding,
not a generic arbitrary JSON field.

The core should preserve `Value`; each provider adapter should own its projection.
Required adapter tests should cover object, array, string, number, boolean, null,
error, and artifact-backed output.

References:

- Gemini FunctionResponse API:
  https://ai.google.dev/api/caching
- OpenAI Responses API, FunctionCallOutput:
  https://developers.openai.com/api/reference/resources/responses/methods/create
- Mistral function-calling guide:
  https://docs.mistral.ai/getting-started/quickstarts/developer/build-an-agent
- Anthropic tool-use documentation:
  https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools

### 7. `context_tokens` is not the current context size

The input-token usage of the last completed model request is useful, but it is not
the exact current context size:

- The assistant response was appended after that request.
- Tool results may have been appended after that request.
- Steering and hook-injected messages may have been appended.
- Providers report cached and uncached tokens differently.
- Some providers may omit usage or report estimated usage.

A compaction snapshot also cannot automatically shrink this gauge. Until another
model request completes, the last reported prompt-token count remains the old
value unless compaction explicitly invalidates or resets it.

Rename the field to something accurate, such as `last_prompt_tokens`. It can be a
strong compaction signal when combined with conservative headroom and a current
size estimate, but it must not be described as exact or self-correcting. Define
how cache-read and cache-write tokens contribute to the prompt-size signal.

Compaction must tolerate a one-turn lag and should trigger before the provider's
hard context limit, not directly at it.

### 8. The child-persistence protocol is incomplete

The proposed trait is too small for the lifecycle it needs to control.

First, `DelegateTool` does not currently receive its tool-call ID through
`ToolContext`, so it cannot construct `{parent_session}:{call_id}` without new
plumbing. This is a cross-crate API change and should be acknowledged.

Second, beginning persistence may involve asynchronous and fallible storage work.
A synchronous `begin() -> ChildRun` does not express that. The protocol must expose
begin failure, append failure, flush completion, and final status.

Third, these two statements contradict each other:

- The child is flushed before `DelegationFinished`, so completion is never visible
  before durability.
- Child persistence failures only warn and degrade without failing delegation.

If flush fails and the parent still emits a normal `DelegationFinished`, the
durable-before-observable guarantee is false. The parent event must either report
that child persistence failed, omit the child-session link, or treat persistence
failure as delegation failure.

Prefer an opaque child-session ID over concatenating parent IDs and call IDs.
Concatenated IDs grow at every nesting level, may violate path constraints, and
make retries append to an earlier child session. Store tenant, parent session,
originating call ID, agent, and depth as explicit metadata.

The protocol also needs:

- Retry and idempotency behavior.
- Concurrent delegations with the same agent.
- Cancellation while flushing.
- Process shutdown while a child is active.
- Recursive parent deletion.
- Child artifact cleanup.
- Nested child handle propagation.
- A defined response when persistence is unavailable.

### 9. Background delegation cannot produce the promised timeline

The proposal keeps background delegation on `TaskSpawned` and `TaskFinished`
instead of emitting delegation events. Those task events do not currently contain
the originating call ID, turn, child usage, or duration. Adding only
`child_session` is insufficient to build the same `DelegationTrace` promised by
the top-level goal.

Choose one consistent model:

1. Emit `DelegationStarted` and `DelegationFinished` for synchronous, parallel,
   and background modes, while task events represent background handle state.
2. Enrich task events with all fields required to produce an equivalent delegation
   edge.

The first option is cleaner. A background task and a delegation are different
facts and do not become duplicate logging merely because they reference the same
execution.

### 10. Timeline correlation needs stronger event keys

The proposed timeline places tools and delegations under turns, but some finish
events omit `turn`, and delegation events rely indirectly on the corresponding
tool call. This makes projection fragile under partial reads, snapshots, old data,
or missing start events.

Carry these fields on both start and finish events:

- `run_id`
- `turn`
- `call_id`
- operation name or delegated agent

The timeline fold must define behavior for:

- Finish without start.
- Start without finish.
- Duplicate starts or finishes.
- Events arriving after run termination.
- Parallel tools finishing out of order.
- Background children finishing after the parent run.
- Snapshot boundaries that cut through active work.

The projection should preserve malformed or incomplete operations as explicit
incomplete traces rather than dropping them.

### 11. Full-log timeline folding needs pagination

`GET /threads/{id}/timeline` cannot indefinitely read and fold the complete event
log on every request. Long-lived threads will make this endpoint O(total events)
in database reads, allocations, CPU, and response size.

Expose run-oriented access:

- A paginated run-summary list.
- A timeline for one run.
- Optional bounded expansion of recent runs.
- Cursor-based event continuation when full detail is required.

The event log can remain the source of truth. Pagination or materialized run
summaries do not violate that principle.

### 12. Thread hierarchy filtering belongs in substrate

Filtering child sessions in serve after fetching a page breaks keyset pagination:
pages can be underfilled, cursors can skip entries, and the backend still reads
irrelevant rows. Root-only, child-only, and parent-specific listing must be part of
the `SessionStore` query contract and implemented by each backend.

The proposal also says thread-list stats can be joined from run rows. The existing
Postgres `runs` table stores operational status and leasing fields, but not token
usage or lifetime stats. State stats are stored inside snapshot JSON. A cheap join
is therefore not currently available.

Production options include:

- Denormalized session-summary columns updated transactionally.
- A dedicated session-summary table.
- Materialized run-usage rows maintained as events are persisted.

Whichever option is selected, define consistency and failure behavior. Do not fold
every session log while serving a thread-list page.

### 13. Grounding is provenance, not yet citation rendering

The proposed `GroundingSource` is useful for audit and source-list UI, but it does
not map statements in an answer to individual sources. Calling it citation
rendering overstates what the structure provides.

Add a stable source ID and, if inline citations are a goal, an explicit attribution
mechanism from output segments to those source IDs. Otherwise call the feature
provenance or supporting sources.

Security and size rules are required:

- Do not persist raw local paths by default.
- Remove credentials and signed query parameters from URLs.
- Bound title, snippet, metadata, and source counts.
- Define tenant-visible versus internal-only metadata.
- Reject or truncate oversized grounding deterministically.
- Treat retrieved snippets as untrusted content.

## Event-model corrections

Before implementation, write down the invariants for every event family.

### Runs

- One `RunStart` per attempt.
- At most one terminal event.
- Cancellation, suspension, failure, and success are distinct typed states.
- Usage is attributed once and cannot be double-counted between turn and run events.
- A process crash leaves an explicitly detectable incomplete run.

### Turns

- Turn numbers are one-based or zero-based everywhere, with one documented choice.
- Each completed provider request produces one `TurnEnd`.
- Provider failure before usage produces an incomplete turn, not fabricated usage.
- Model duration is measured around provider execution only.
- Decide whether to capture time-to-first-token separately from total model time.

### Tools

- Every model-requested call has a durable disposition.
- Every actual execution has one start and one finish.
- Substitution and guard blocking are not presented as real execution latency.
- Unknown, timeout, panic, cancellation, and tool-declared errors remain distinct.

### Delegations

- Every delegation mode produces a navigable edge.
- Parent and child usage are never double-counted.
- Child persistence state is visible on the edge.
- Parent deletion has defined recursive semantics.

## Statistics corrections

Counting runs from `RunStart` is sensible, but successful runs should not be
derived with unchecked subtraction. Corrupt, duplicated, migrated, or overlapping
terminal events could underflow or produce nonsense. Fold typed terminal states
directly into explicit outcome counters, or use saturating arithmetic while also
recording an invariant violation.

Per-model accounting should be included now. Once events are durable, adding model
identity later leaves old events unattributable. `TurnEnd` should carry the actual
provider and model used, including fallback selection. Delegation edges should
carry the child's actual provider/model attribution or refer to the child run that
does.

Cache-token fields should also be added before durable v2 logs accumulate. Define
their semantics across providers rather than assuming every provider reports the
same categories.

Avoid unbounded maps in `ThreadStats`. Tool and model names may be application-
controlled and high-cardinality. Set a policy for maximum tracked entries, an
`other` bucket, or keep detailed breakdowns in store-side queries while retaining
only bounded totals in state.

## Expanded test requirements

The proposed test plan is a good start but is not sufficient for the new failure
surface.

### Terminal-state tests

- Failure in every before/after agent hook.
- Failure in every before/after model hook.
- Failure in every before/after tool hook.
- Provider preparation, provider call, decoding, and fallback failure.
- Tool timeout, panic, unknown tool, and cancellation.
- Exactly one terminal event on every handled path.
- No terminal event duplication during resume or retry.
- Truncated/crashed runs remain visibly incomplete.

### Timing and ordering tests

- Serial tool durations exclude earlier tools.
- Parallel tools have independent start times and durations.
- Finish order may differ from request order without corrupting the timeline.
- Monotonic durations remain valid if wall-clock time moves backwards.
- Background completion after parent termination remains linkable.

### Structured-output tests

- Object, array, string, number, boolean, and null outputs.
- Provider-specific request encoding for every output type.
- Structured error outputs where supported.
- MCP and Composio preserve JSON rather than stringify and reparse internally.
- Hooks can inspect structure without changing provider encoding.

### Artifact-retention tests

- Inline-to-artifact threshold boundaries.
- Artifact write failure.
- Event write failure after artifact success.
- Restart between artifact write and event append.
- Missing, deleted, foreign-tenant, and foreign-session artifact references.
- Artifact resolution preserves `tool_use_id`.
- Parent and child deletion remove artifact bytes and metadata.
- Orphan cleanup is idempotent.

### Timeline and fold tests

- Duplicate, missing, and out-of-order lifecycle events.
- Snapshot in the middle of an active turn, tool, task, or delegation.
- Fold determinism across arbitrary valid event sequences.
- Malformed sequences produce incomplete traces instead of panics.
- Pagination produces every run exactly once.
- Full-log and paginated/run-scoped projections agree.

### Child-persistence tests

- Handle absent preserves ephemeral behavior.
- Begin failure, append failure, and flush failure.
- Persistence failure is reflected accurately on the parent edge.
- Retry does not append two transcripts to one accidental child ID.
- Concurrent children remain isolated.
- Correct tenant identity with and without persistence.
- Nested delegation creates a walkable hierarchy.
- Parent deletion recursively removes descendants and their artifacts.
- Root listing excludes children without breaking pagination.

### Security tests

- Run config secrets never enter the event log.
- Grounding URLs are stripped of credentials and signed parameters.
- Local paths are not exposed by default.
- Oversized grounding, tool output, and audit metadata follow bounded policies.
- Child lookup and timeline access enforce tenant ownership.

## Recommended delivery split

### Phase 1: durable execution facts

Implement the corrected run, turn, tool, and delegation event model first. Add
typed statuses, exactly-once terminal finalization, per-operation timing, provider
and model attribution, bounded stats, and paginated timeline projections.

This phase should not include artifact retention or persistent child transcripts.
It can still persist delegation edges and child usage from ephemeral execution.

### Phase 2: structured tool output and retention

Introduce `serde_json::Value`, provider-specific projection, grounding/provenance,
artifact-backed tool-result payloads, spill policy, failure semantics, and garbage
collection as one coherent migration.

### Phase 3: persistent child sessions

Add the asynchronous child-persistence lifecycle, opaque child-session identity,
hierarchy metadata, store-level list filters, recursive deletion, nested
propagation, and child artifact cleanup.

## Immediate action before the phases

Fix the default child identity so every child inherits the parent tenant even when
child persistence is disabled. This is an isolation correction, not a State v2
feature.

## Acceptance gate

State v2 is ready for implementation when the following questions have explicit,
testable answers:

1. What is the exactly-once terminal-event invariant?
2. What values may enter the persisted run audit stamp?
3. What statuses distinguish requested calls from actual executions?
4. How does an artifact-backed tool result retain its tool-call identity?
5. How does each provider encode every JSON value category?
6. What happens on artifact and event partial failure?
7. What does the compaction token signal actually measure?
8. How does child persistence report begin and flush failure?
9. How are child retries, deletion, and artifacts handled?
10. How are timelines paginated without folding an unbounded log per request?
11. How do background delegations produce the same navigable execution edge?
12. How are high-cardinality stats bounded?

Until those are resolved, implementation would likely lock ambiguous behavior into
the durable log format and make later corrections substantially more expensive.
