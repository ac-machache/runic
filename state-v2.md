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
      (`cachedContentTokenCount`); Mistral fills read
      (`prompt_tokens_details.cached_tokens`, added in the 1.7 review round).
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
      `dispatch_one` returns `Dispatched { result, status, duration_ms }`
      with the monotonic timer wrapping exactly one execution (shared by serial
      + parallel paths); durable `ToolStarted` events are stamped in the
      announce phase BEFORE any execution (deliberate: crash evidence — for
      serial batch-mates `at` is batch start, `duration_ms` is the accurate
      per-tool measure) and `ToolFinished { ToolStatus, duration_ms }` is
      pushed in the collect phase.
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

- [x] **2.1 `ToolResult` becomes an outcome enum** (`runic-tool`) — DONE.
      `Done { output: Value, provenance, retention } | Failed { message } |
      Deferred { channel, payload }`; old `Deferral` struct removed
      (`PendingDeferral` is dispatch-internal in `runic-agent`). Constructors
      `ok(impl Into<Value>)`/`error`/`defer` preserved every in-tree call site;
      helpers `is_error()`/`output()`/`text()`/`push_notes()` replaced all
      flag reads. Loop-guard nudges: string outputs get appended text,
      structured outputs get wrapped once as `{ "output": …, "notes": […] }`.
      `runic-tool` now depends on `runic-types`. Dispatch consumes by match —
      no flag cross-checking anywhere.

- [x] **2.2 Provider projection of `Value`** (`runic-provider`) — DONE.
      Mistral reference rule: inline strings verbatim, any other value compact
      JSON (`ToolResultPayload::text()`), `(empty)` padding kept; OpenAI
      (shared `tool_result_text`, both chat paths) and Anthropic follow the
      same stringify rule; Gemini passes objects natively into
      `FunctionResponse.response` and wraps every non-object (array, string,
      number, bool, null, artifact preview) in `{ "result": value }`.
      Per-driver category tests over object/array/string/number/bool/null/
      error/artifact assert the request bodies (146 provider tests green).

- [x] **2.3 Tool-result payload block** (`runic-types`) — DONE.
      `ContentBlock::ToolResult.content: ToolResultPayload` =
      `Inline(Value) | Artifact { id, preview, mime, size }`, serialized
      externally tagged (`{"inline": …}` / `{"artifact": {…}}`), plain
      tagged derive both ways — NO legacy fallback (old logs get wiped, per
      standing no-compat order). Block also gained `provenance`
      (serde-default, skip-if-empty). Round-trip proptest extended over both
      arms incl. the adversarial `{"inline": …}` output case; msgpack + JSON
      round-trips pinned; `tool_use_id` untouched.

- [x] **2.4 Provenance** (`runic-types` + dispatch + wire) — DONE.
      `ProvenanceSource { id, source, title, snippet, metadata }` in
      `runic_types::provenance` (re-exported from `runic-tool`);
      `sanitize_provenance` applied in dispatch before persist/emit: max 16
      sources, URL userinfo + secret query params stripped (sig/token/key/…,
      `x-amz-*`, `x-goog-*`), local paths redacted to `file:<basename>`,
      char-boundary truncation (id 128 / source 2048 / title 256 / snippet
      1024), metadata > 2048 serialized bytes dropped. Persisted on the block,
      carried on `AgentEvent::ToolFinished` and wire `tool_finish`
      (skip-if-empty), and per-driver tests assert absence from every
      provider request body.

- [x] **2.5 Retention + spill** (`runic-agent`) — DONE.
      `Retention: Full | Summary(Value) | Artifact`. `Summary` keeps the
      transient-overlay semantics (`with_persisted_summary` renamed
      `with_summary`; overlay now holds `Value`). `Artifact`: new
      `ToolOutputSpill` seam (`runic-agent`), `AgentBuilder::artifact_spill` +
      `auto_spill_over(bytes)`; the umbrella crate wires it from
      `Composer::artifacts(store)` via `SpillToArtifacts`
      (`ArtifactSource::ToolOutput`), composer knob `auto_spill_over`.
      Serialization rule: string output → raw UTF-8 (`text/plain`), any other
      value → compact JSON (`application/json`); `auto_spill_over` measures
      those bytes and only fires on `Full` retention. History keeps the
      `Artifact` payload arm (256-char preview); on a normal (non-suspending)
      batch the model still gets the full output on the immediate next call
      via the overlay; on suspension it retrieves the payload via the
      auto-wired `read_thread_artifact` tool (`Composer::artifacts(store)`
      registers it unless the consumer already owns the name). Spill failure
      (or no store wired) degrades to an inline `[artifact spill failed: …]
      preview` note — never an unbounded inline write. Tests: `runic-agent/
      tests/retention.rs` + `runic/tests/suspend_retrieve.rs` (spill
      round-trip incl. tenant/session/mime/bytes, failure fallback,
      absent-store fallback, threshold behavior, provenance
      sanitize+persist+wire, suspend → resume → retrieve e2e). Orphan GC
      stays with the artifact store's existing delete/list surface (Phase 3
      ties it to child cleanup).

- [x] **2.6 MCP + Composio preserve JSON** (`runic-mcp`, `runic/src/tools`) —
      DONE. `CallToolResult` gained `structured_content`
      (`structuredContent`); `McpTool::execute` returns it as the `Value`
      output when present (text fallback unchanged, errors stay text);
      Composio `execute` returns the raw response `Value` instead of
      pretty-printed text. Scripted-transport tests pin structured, text-only,
      and error shapes.

**Phase 2 exit gate:** gate question 4 answered (artifact result keeps
tool-call identity — payload arm lives inside the `tool_result` block,
`tool_use_id` asserted in provider tests); gate question 5 answered
(per-provider encoding of every JSON category — per-driver tests); gate
question 6 PARTIALLY answered: spill-store failure degrades to a bounded
inline note (tested), but the artifact-store write paths themselves still
have partial-write windows (Postgres: bytes before metadata; Local: blob →
meta → index) and event persistence can fail after an artifact write — full
partial-failure safety needs the pending/committed lifecycle + orphan
sweeper parked to Phase 3 (3.4). NOTE: `runic-memory` was removed from the
workspace (moved to `../runic-memory` as an archive) and the memory domain
(Memory ability, MemoryCurator hook, `runic::memory` re-export) was cut from
the umbrella crate — memory becomes a consumer-owned ability.

---

## Phase 3 — persistent child sessions

- [x] **3.1 `ChildPersistence` protocol** — DONE. `runic_state::child`:
      `ChildPersistence::begin(agent) -> Box<dyn ChildSink>` (async +
      fallible), `ChildSink { session_id, sink() -> PersistSink, nested() ->
      ChildPersistenceHandle, flush() -> Result }`; `ChildPersistenceHandle`
      newtype rides the `ToolContext` bag. Injected via
      `RunContext::with_child_persistence` (serve wires it on all four run
      modes when the factory is stateful); `DelegateTool` picks it up in
      `child_ctx`. Handle absent → ephemeral, unchanged (all prior delegate
      tests pass untouched). Serve impl (`runic-serve/src/child.rs`):
      opaque `chd-{uuid}` id, per-child persister with BOUNDED retries (5) —
      give-up marks the sink failed so `flush()` returns the store error.
      Tests: `runic/tests/child_persistence.rs` (transcript under opaque id,
      begin-failure → ephemeral, flush-failure edge, fresh id per attempt) +
      `runic-serve/tests/child_persistence.rs` (real store round-trip,
      begin failure, bounded-retry flush failure, nested() grandchild rows).

- [x] **3.2 Opaque child identity + hierarchy metadata** — DONE. Child ids are
      opaque (`chd-{uuid}`, minted by the serve impl; the interim
      `{parent}:{def}` string survives only as the ephemeral fallback);
      `SessionMeta` gains `agent` + `parent_session`;
      `SessionStore::create_child_session` + `SessionScope { Roots (default),
      ChildrenOf, All }` on `list_sessions_page` — WHERE clause in Postgres
      (migration `0008_child_sessions.sql`: columns + parent index), filter
      in memory, keyset-safe. Contract test
      `child_sessions_carry_hierarchy_and_scope_listings` (roots exclude
      children; children-of pages cover every child once).

- [x] **3.3 Honest edges** — DONE. `begin()` runs BEFORE `emit_started`, so
      `DelegationStarted.child_session` is set only when persistence actually
      began; `DelegationFinished` gains `child_session` +
      `child_persistence: Option<ChildPersistenceStatus{Flushed|FlushFailed}}`;
      `run_child` flushes the sink before the edge is emitted on every path
      (success and child failure). Wire `delegation_start`/`delegation_finish`
      carry `child_session` (+ `child_persisted` bool); timeline
      `DelegationTrace.child_session` fills from either edge; replay proptest
      extended over the new fields.

- [x] **3.4 Lifecycle semantics** — DONE. Retry idempotency (fresh opaque id
      per attempt, tested), concurrent children isolated (per-sink channels),
      nested handle propagation e2e (`nested()` re-scoped to the child; a
      running mid-agent delegates to a leaf — three-level hierarchy walkable,
      the child's own transcript carries its delegation edge pointing at the
      grandchild's session), recursive parent deletion via serve
      `DELETE /threads/{id}` (walks `ChildrenOf` pages, deletes leaf-first
      incl. each session's artifacts — grandchild-deep test). Cancellation
      while flushing: `ChildSink::flush` is deadline-bounded (15s) on top of
      the bounded-retry persister, so a hung store cannot hang a delegation —
      paused-time test proves the timeout and reports the backlog. Process
      shutdown with an active child: the child's dangling `RunStart` projects
      as `InFlight` and the child stays reachable via `ChildrenOf` (tested).
      Artifact GC: `ArtifactStore::sweep_orphans(tenant, session, older_than)`
      (default 0 for single-layer backends); `PostgresArtifactStore` diffs the
      inner byte store against the metadata rows and deletes unindexed bytes
      older than the cutoff — idempotent, never races an in-flight `put`
      (live-Postgres test). Scheduling the sweep (a maintenance endpoint or a
      reaper-style background task in serve) is left to the consumer for now.

- [x] **3.5 Serve exposure** — DONE. `GET /threads/{id}/children`
      (keyset-paginated, `ThreadSummary` gains `agent` + `parent_thread`,
      404 on unknown/foreign-tenant parent); child timelines/events/state are
      reachable through the existing thread endpoints since a child IS a
      session (tenant scoping applies everywhere). Test: children listed +
      excluded from the root thread list + cross-tenant rejected + recursive
      delete removes the whole tree.

**Phase 3 exit gate:** gate questions 8 (begin/flush failure reporting),
9 (retries, deletion, artifact cleanup).

### Phase 3 review round (post-landing)

Fixed: deleted children can no longer be resurrected as root threads by a
late persister batch — `SessionStore::append_batch_strict` (memory atomic,
Postgres row-lock, default check-then-append) fails `NotFound` instead of
upserting the sessions row; the child persister uses it and gives up
immediately on `NotFound` (no pointless retries), so the edge reports
`FlushFailed` and the tree stays clean (contract test on every backend +
serve race test). A flush timeout now also STOPS the writer: the timeout
marks the sink failed and fires a `watch` stop signal the persister selects
on between batches and during retry backoff (the one in-flight batch may
still land; nothing enqueued after the timeout does — paused-time test).
The stop sender lives on the shared progress state, not the sink, so a
normal sink drop never kills a draining persister. `sweep_orphans` cutoffs
are clamped to a store-level margin (default 5 min,
`with_sweep_margin` to override) so a small `older_than` can never race an
in-flight put. Parallel and background delegation edges are now pinned by
tests (previously sync-only — background exercises the out-of-run
`ExternalEvents` path). Memory-store session listing gained the
`session_id` keyset tiebreak (identical `last_activity` could skip a child
during paged walks). OpenAI got the missing provenance-absence request-body
test; its `complete`/`stream` message conversion was deduplicated into one
`build_messages` (the stream path had drifted and silently dropped user
image/file parts).

Round 2 (both former known-opens closed): `DELETE /threads/{id}` is now
fenced by the thread lease — it claims with a distinct `delete:{instance}`
owner (the lease is owner-reentrant, so reusing the instance id would not
fence same-instance runs), returns 409 while a run holds the lease, and
releases on failure; a run admitted while the delete holds the lease aborts
at `claim_run` on its deleted run row (`Claim::Lost`), so it never writes.
`create_run` materializes the sessions row in the same transaction (memory
mirrors), so the parent row provably exists before any delegation can
begin; `create_child_session` then fails `NotFound` under a missing parent
(Postgres `FOR SHARE` same-tx check, memory atomic) — orphan child rows
can no longer be created. Residual orphans (from any historical or exotic
path) are reaped by `SessionStore::delete_orphan_children` — a fixpoint
sweep (reaping an orphan may orphan its children) returning the ids so
serve can delete their artifacts — which serve runs best-effort after
every tree delete. Contract tests on both backends + a serve 409/204
fence test.

---

### Phase 2 review round (post-landing)

Fixed: transient overlay is occurrence-scoped (`Vec<Value>` per call id,
newest-to-newest pairing on the next request — duplicate call ids can no
longer receive each other's output); a suspending batch spills summarized
outputs to the artifact store instead of losing them to the in-memory
overlay (no store → summary kept, full bytes never inline, warn);
`auto_spill_over` now also bounds Summary and Failed inline payloads
(deterministic truncation with a byte-count marker) and spill-failure notes
truncate the backend error to 200 bytes; provenance URL sanitization uses
`url::Url` (case-insensitive schemes, percent-decoded query names,
unparseable http(s) URLs fail closed to `[unparseable-url]`); providers
without a native error bit mark failed results (OpenAI/Mistral: `Error: `
prefix; Gemini: `{ "error": text }` envelope; Anthropic keeps `is_error`).

Round 2 fixes: suspension-spilled summaries use the tool's SUMMARY as the
artifact preview and failure-note text (the full output can no longer leak
through the 256-char preview); suspension semantics documented honestly —
after resume the model sees the summary preview + artifact id and retrieves
the payload via `read_thread_artifact`; it is NOT auto-inlined into the next
request (the resolver only handles `ContentBlock::ArtifactRef`, not payload
arms). `bounded_inline` is now a hard byte ceiling (marker length reserved
inside the budget, spill-failure notes included). `has_scheme_ci` compares
bytes (no UTF-8 boundary panic on non-ASCII sources). Provenance metadata is
scrubbed recursively — object keys matching the secret-param list are
removed before persistence. Postgres artifact deletes run bytes-first and
tolerate missing bytes, so a retried delete converges instead of stranding
invisible orphans. Backward-compat removal (user order — no old-log
compatibility, ever): `ToolResultPayload` deserialization is the plain
tagged derive; the legacy string fallback and its test are gone.

Round 3 fixes: `Composer::artifacts(store)` now auto-registers
`ReadThreadArtifactTool` (skipped when the consumer already owns the name),
its description covers spilled tool outputs as well as user uploads, and
`runic/tests/suspend_retrieve.rs` proves the full suspend → resume →
retrieve loop end-to-end. The exit-gate wording for question 6 was corrected
to "partially answered" (put-path partial-write windows + the
artifact-then-event failure window remain open until Phase 3's
pending/committed lifecycle + sweeper).

Rejected: replayed `tool_finish` carrying provenance (payload discipline —
ground rule 5: lifecycle events carry identity/timing/status only; replayed
`tool_finish` already has an empty preview by design, and the replayed
Message event carries the block with provenance); a payload version
discriminator (legacy content was a string *by type*, so old logs can only
contain strings; new writes are always tagged, round-trip identity is
proptested including adversarial `{"inline": …}` outputs).

Parked to Phase 3: artifact/event partial-failure windows + orphan sweeper
(pending/committed artifact state, idempotent GC) — joins child-artifact
cleanup in 3.4.

## Parked (explicitly out of all three phases)

- Live child streaming into the parent SSE → the stream-detail-flag design
  (Lean/Standard/Full verbosity on `RunContext`).
- State namespacing (`app:/user:` prefixes) — cross-session state remains
  runic-memory's job.
- `SessionStore::usage(tenant, from, to)` — per-tenant billing rollup query
  (roadmap item; store-side, not a fold).
