# State v2 — execution plan

Working plan derived from `design-v2.md` + `critique-v2.md` + review discussion.
Tasks are ordered by phase; a phase starts only when the previous phase's exit
gate is answered with tests. Anyone (human or agent) picking up a task: check it
off with a note of where the work landed.

## Ground rules

1. **Provider tiebreaker: Mistral wins.** When a provider-facing decision is hard
   (encoding shapes, usage semantics, field availability), pick the option that
   suits the Mistral provider best; other providers adapt in their own drivers.
2. The event log is append-only and the fold is deterministic — every new event
   variant extends the fold proptest before it ships.
3. Beta: no compat shims, dev logs get wiped. But ambiguity is forever once the
   durable format lands — no field ships without defined semantics.
4. Repo conventions apply (`CLAUDE.md`): no new comments, sparse modules, tests
   green + `cargo clippy --workspace --all-features --tests -- -D warnings` clean
   before a task is checked off. Never commit without an explicit order.
5. Payload discipline: message blocks carry payloads (args, results); lifecycle
   events carry identity, timing, and status only. No double storage.

---

## Phase 0 — immediate isolation fix (independent of v2)

- [x] **0.1 Child tenant identity** — `DelegationCtx` carries parent `tenant` +
      `session`; default `SubagentBuilder::identity()` inherits them instead of
      `("subagent", def.name)`. Landed in `runic-subagent/src/delegate.rs`,
      pinned by `children_inherit_the_parent_tenant` in `crates/runic/tests/child.rs`.
      Child session string `{parent}:{def}` is interim — replaced by opaque ids
      in Phase 3; nothing durable depends on it.

---

## Phase 1 — durable execution facts

No structured output, no artifacts, no child transcripts. Delegation *edges* and
child usage land here (ephemeral children).

- [x] **1.1 `TokenUsage` gains cache fields** (`runic-types`) — DONE.
      `cache_read_tokens`/`cache_write_tokens`, serde-defaulted; values are
      provider-reported verbatim, no cross-provider normalization, never
      estimated. Anthropic fills both (`cache_read_input_tokens`/
      `cache_creation_input_tokens`, streaming too); OpenAI fills read
      (`prompt_tokens_details.cached_tokens`); Gemini fills read
      (`cachedContentTokenCount`); Mistral reports none → zeros.
      `TokenUsage::add()` added for accumulation. Tested per driver + old-log
      deserialization in `runic-types`.

- [x] **1.2 `SessionEvent` v2 variants** (`runic-state`) — DONE. All variants below
      landed; `TurnBoundary` removed; `TurnEnd` emitted live with real per-turn
      usage, actual model (fallback-aware via `call_model` returning the served
      model), and measured `model_ms`; `RunEnd` carries typed status on both the
      success and error paths (suspension emits no terminal event); replay
      proptests extended over every new variant (round-trip + determinism);
      substrate/postgres kind mapping, wire filtering, and all fixtures updated.
      Tool/delegation event EMISSION lands in 1.5/1.6.
      - `RunStart { run_id, agent, audit: Option<AuditStamp>, at }`
      - ONE typed terminal event: `RunEnd { run_id, status, outcome, at }` with
        `status: Completed | Failed(String) | Cancelled` — `RunError` variant
        dropped in favor of typed status (single-finalizer friendly).
      - Suspension is NOT terminal: a suspended run has no `RunEnd`; resume
        continues the same `run_id` and emits no second `RunStart`.
      - `TurnEnd { run_id, turn, model: String, usage, model_ms, at }`
        (replaces `TurnBoundary`; carries the actual model used, incl. fallback).
      - `ToolStarted { run_id, turn, call_id, tool, at }`
      - `ToolFinished { run_id, turn, call_id, tool, status: ToolStatus, duration_ms, at }`
        `ToolStatus: Ok | ToolError | ExecError | Timeout | Cancelled | UnknownTool | GuardBlocked | Substituted`
      - `DelegationStarted { run_id, turn, call_id, agent, mode, at }` and
        `DelegationFinished { run_id, turn, call_id, agent, status, usage, model: Option<String>, duration_ms, at }`
        — emitted for **all three modes** (sync, parallel, background); Task
        events remain background *handle state* only.
      - Correlation keys (`run_id`, `turn`, `call_id`, name) on BOTH start and
        finish events — finish events are locally useful under truncation.
      *Accept:* fold proptest extended over all variants; event invariants from
      `critique-v2.md` §"Event-model corrections" written as tests.

- [x] **1.3 `AuditStamp` — typed, allowlisted** — DONE. `AuditStamp { model, actor }`
      (agent stays first-class on `RunStart`); `RunContext::with_actor(...)` is the
      only way in, truncated to 128 chars at emission; the open config map has no
      path into the log by construction. Pinned by
      `the_audit_stamp_carries_actor_and_model_but_never_the_config_map` in
      `event_order.rs` (secret-laden config, asserts absence across ALL events).

- [x] **1.4 Single run finalizer** — DONE. `run_loop` split into
      `run_loop_inner` (all fallible work: BeforeAgent hooks, the loop,
      AfterAgent hooks) + `finalize_run` — the ONLY place a terminal `RunEnd`
      is emitted, exactly once per exit path; suspension stays non-terminal.
      The two escape paths (BeforeAgent-hook failure after `RunStart`,
      AfterAgent-hook failure before `RunEnd`) now funnel to `Failed`.
      Tests: hook-Stop injected at each lifecycle point → exactly one Failed
      terminal (`event_order.rs`); provider failure → one Failed terminal;
      suspend+resume → exactly one Completed terminal, no second `RunStart`
      (`suspend.rs`).

- [x] **1.5 Tool instrumentation at the execution boundary** — DONE.
      `dispatch_one` returns `Dispatched { result, status, started_at, duration_ms }`
      with the monotonic timer wrapping exactly one execution (shared by serial
      + parallel paths); durable `ToolStarted` (true start timestamp) +
      `ToolFinished { ToolStatus, duration_ms }` pushed in the collect phase.
      `ToolStatus` gained `Panic`. Substituted/guard-blocked/cancelled calls get
      a `ToolFinished` disposition with zero duration and NO `ToolStarted`;
      deferred calls get `ToolStarted` + `ToolDeferred` as their disposition.
      `dispatch_tools` carries `turn`. Tests: `tool_events.rs` (serial sleeper —
      fast tool's duration excludes the slow one's; unknown-tool distinct
      status) + all event-order pins updated.

- [x] **1.6 Delegation edges from `DelegateTool`** — DONE. `run_child` returns
      `ChildRun { text, usage, model }`; `emit_started`/`emit_finished` fire via
      `ExternalEvents` for sync, parallel, and background (background edge lands
      when the task finishes, Task events unchanged). Correlation via new
      `runic_tool::CallId`/`CurrentTurn` bag entries inserted per call in
      dispatch. Pinned by `delegation_edges_land_in_the_parent_log_with_child_usage`
      (mode/turn/status/usage/model asserted; parent usage excludes child's).

- [x] **1.7 `ThreadStats` v2** — DONE. Attempts from `RunStart`,
      errored/cancelled from typed `RunEnd`; own tokens (incl. cache) +
      `model_ms` + `tokens_by_model` (bounded 16 + `other_models`) from
      `TurnEnd`; TOKEN CONTRACT (post-review): `input_tokens` = TOTAL prompt
      tokens including cached, on every provider (Anthropic normalized by
      adding its separately-reported cache tokens in; OpenAI/Gemini/Mistral
      already inclusive); `cache_read/write_tokens` = informational subsets;
      `last_prompt_tokens` gauge = `input_tokens`, exact everywhere; Mistral
      adapter reads `prompt_tokens_details.cached_tokens` (both paths);
      per-tool `ToolStat { calls, errors, total_ms, max_ms }`
      bounded 64 + `other_tools`, counting moved off message-block digging;
      delegation counters + `delegated_usage`. `CompactionHook` consumes the
      gauge (`max(chars/4-estimate, gauge)`). `StateSnapshot.stats` boxed.
      **Hydrate replay is now a pure fold of the full log** — the old
      RunStart-skip allowlist removed; a crashed run's dangling `RunStart`
      stays visibly in-flight (gate #1 semantics), run rows own orphan
      handling. Fold tests: attempts/outcomes, per-model + gauge, tool
      latency/errors, bounded-map overflow, delegation separation.

- [x] **1.8 `timeline()` projection** — DONE. `runic_state::timeline::project`
      (pure fn over any event iterator — serve reuses it on stored logs) +
      `AgentState::timeline()` / `timeline_for(run_id)`. Tree:
      `RunTrace { agent, audit, status incl. InFlight, usage } → TurnTrace
      { model, usage, model_ms, complete } → ToolTrace (start-less substituted
      + finish-less hanging both preserved, deferred flag) / DelegationTrace`.
      First-write-wins on duplicates; turn buckets self-create from correlation
      keys. Tests: golden exact-tree, incomplete-preservation, and a
      shuffle/truncation proptest (no panic, no finished tool ever dropped).

- [x] **1.9 Serve: wire + timeline endpoints** — DONE (minus stats-on-`done`,
      see below). Wire: `TurnComplete` extended (optional durable
      usage/model/`model_ms` on replay, `stop_reason` now optional),
      `ToolFinished` replays as `tool_finish`, new `delegation_start`/
      `delegation_finish` wire events (live thread-events stream + replay both
      go through `from_session_event`). Endpoints:
      `GET /threads/{id}/runs?limit&before` (keyset-paginated run summaries via
      new `SessionStore::list_runs`, memory + postgres impls) and
      `GET /threads/{id}/runs/{run_id}/timeline` (run-scoped
      `read_run_after` → `timeline::project`, never folds the whole log).
      OpenAPI registered. End-to-end test drives a run then asserts list +
      tree. DEFERRED: stats on the `done` wire event (needs post-run state
      plumbing into the SSE fan; stats stay reachable via thread state).

- [x] **1.10 Substrate: thread-list summaries** — DONE. Denormalized columns on
      the sessions row (user call: columns, no new tables): `run_count`,
      `errored_runs`, `input_tokens`, `output_tokens`, `last_run_status`
      ("running"/"completed"/"failed"/"cancelled"), `last_run_at`. Shared
      `summary_delta(event)` in `runic-substrate/src/sessions.rs`; Postgres
      bumps them in the SAME transaction as the event insert (migration
      `0007_session_summaries.sql`); memory backend mirrors. `SessionMeta`
      carries the fields; serve `ThreadSummary` exposes them — the thread list
      is one indexed query, zero log folds. Contract test
      `summary_columns_track_runs_and_tokens` runs against every backend.

**Phase 1 exit gate** (from `critique-v2.md`): answers with tests for gate
questions 1 (terminal invariant), 2 (audit stamp contents), 3 (call vs execution
statuses), 7 (what the compaction signal measures), 10 (timeline pagination),
11 (background edges), 12 (stats bounding).

---

## Phase 2 — structured tool output + retention

One coherent migration; starts only after Phase 1 events are durable.

- [ ] **2.1 `ToolResult` becomes an outcome enum** (`runic-tool`)
      `Done { output: Value, provenance, retention } | Failed { message } |
      Deferred { channel, payload }`. Constructors preserve call sites:
      `ok(impl Into<Value>)`, `error`, `defer`, `.with_provenance`,
      `.with_summary`, `.spill()`. Hooks/dispatch/stats consume by match —
      the `success`/`error`/`deferred` flag combinations become unrepresentable.
      *Accept:* every in-tree tool compiles with constructor-only changes;
      dispatch has no flag cross-checking left.

- [ ] **2.2 Provider projection of `Value`** (`runic-provider`)
      Core preserves `Value`; each driver owns its projection. **Mistral is the
      reference implementation and its needs win ties**: structured results are
      serialized to a JSON string for the tool message (Mistral's documented
      shape); OpenAI + Anthropic follow the same stringify rule; Gemini wraps
      non-objects in `{ "result": value }` for `FunctionResponse.response`.
      *Accept:* per-driver tests over object, array, string, number, boolean,
      null, error, artifact-backed — request bodies asserted byte-for-shape.

- [ ] **2.3 Tool-result payload block** (`runic-types`)
      `ContentBlock::ToolResult.content` becomes a payload that preserves
      `tool_use_id` while being `Inline(Value)` or
      `Artifact { id, preview, mime, size }`. Old logs (string content)
      deserialize as `Inline(Value::String)`.
      *Accept:* round-trip tests old→new; provider projection handles both arms;
      `tool_use_id` correlation asserted end-to-end.

- [ ] **2.4 Provenance** (rename from grounding) (`runic-tool` + wire)
      `ProvenanceSource { id, source, title, snippet, metadata }` — stable ids;
      it is provenance/supporting-sources, NOT inline citations (no segment
      attribution claimed). Security: no raw local paths by default, credentials
      + signed query params stripped from URLs, bounded counts/sizes,
      deterministic truncation, snippets treated as untrusted. Persisted on the
      block, surfaced on the wire, never sent to any model.
      *Accept:* security tests from `critique-v2.md`; per-driver tests assert
      absence from every provider request.

- [ ] **2.5 Retention + spill** (`runic-agent` + `runic-substrate`)
      `Retention: Full | Summary(Value) | Artifact`. `Summary` keeps today's
      transient-overlay semantics (full output exactly once). `Artifact`: write
      to `ArtifactStore` (dispatch gets an explicit store handle on
      `AgentBuilder`), history keeps the payload block's `Artifact` arm.
      Defined: serialized-UTF-8 bytes measured for `auto_spill_over`; artifact
      write failure → fall back to `Summary` with error note, never an unbounded
      inline write; artifact-then-event partial failure; tenant/session
      ownership on refs; size caps; orphan GC (idempotent).
      *Accept:* artifact-retention test list from `critique-v2.md` in full.

- [ ] **2.6 MCP + Composio preserve JSON** (`runic-mcp`, `runic/src/tools`)
      Native JSON results stop being stringified-then-reparsed.
      *Accept:* MCP structured content lands as `Value` end-to-end.

**Phase 2 exit gate:** gate questions 4 (artifact result keeps tool-call
identity), 5 (per-provider encoding of every JSON category), 6 (partial-failure
behavior).

---

## Phase 3 — persistent child sessions

- [ ] **3.1 `ChildPersistence` protocol** (`runic-state` trait, `runic-serve` impl)
      Async + fallible: begin failure, append failure, flush completion, final
      status all expressed. Injected via `RunContext` (serve:
      `build_run_context`); picked up by `DelegateTool` via `ctx.get`; absent →
      ephemeral (unchanged). Requires `call_id` plumbing into `ToolContext`
      (acknowledged cross-crate change).
      *Accept:* handle-absent path byte-identical to today; begin/append/flush
      failures each tested.

- [ ] **3.2 Opaque child identity + hierarchy metadata** (`runic-substrate`)
      Child session id is opaque (uuid); `SessionMeta` gains `agent` +
      `parent_session`; tenant, parent session, originating `call_id`, agent,
      depth stored as explicit metadata. Store-level list filters: root-only
      (default), children-of(parent) — part of the `SessionStore` contract,
      implemented per backend, keyset-pagination safe.
      *Accept:* root listing excludes children without underfilled pages;
      contract tests across memory + postgres backends.

- [ ] **3.3 Honest edges** (`runic-subagent`)
      `DelegationStarted.child_session` set only when persistence began;
      `DelegationFinished` carries child persistence status — flush failure is
      visible on the edge (no false durable-before-observable). Child flushed
      before the parent's `DelegationFinished` on the success path.
      *Accept:* flush-failure test asserts the edge reports it; ordering test
      asserts child durability precedes edge visibility.

- [ ] **3.4 Lifecycle semantics** (`runic-subagent` + `runic-substrate`)
      Retry idempotency (a retried delegation never appends to a previous
      child's transcript — new opaque id per attempt), concurrent same-agent
      children isolated, cancellation while flushing, process shutdown with an
      active child (detectably incomplete), recursive parent deletion removes
      descendants + their artifacts, nested handle propagation.
      *Accept:* child-persistence test list from `critique-v2.md` in full.

- [ ] **3.5 Serve exposure** (`runic-serve`)
      Children listing per thread, child timeline access, tenant ownership
      enforced on every child lookup.
      *Accept:* cross-tenant child access rejected; child timeline reachable
      from the parent edge.

**Phase 3 exit gate:** gate questions 8 (begin/flush failure reporting),
9 (retries, deletion, artifact cleanup).

---

## Parked (explicitly out of all three phases)

- Live child streaming into the parent SSE → the stream-detail-flag design
  (Lean/Standard/Full verbosity on `RunContext`).
- State namespacing (`app:/user:` prefixes) — cross-session state remains
  runic-memory's job.
- `SessionStore::usage(tenant, from, to)` — per-tenant billing rollup query
  (roadmap item; store-side, not a fold).
