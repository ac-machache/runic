# Bug Tracker — runic

Running log of bugs, design gaps, limitations, and the fixes/suggestions we
landed on while testing the coral ("Maia") agent end-to-end.

Status legend: 🔴 open · 🟡 in progress · ✅ fixed · 🔵 design decision · ⚪ deferred

---

## 🛠 Cross-cutting suggestion: boot-time validation + warnings

Several bugs below only surface at *runtime* (mid-conversation) when they
could be caught when the server assembles the agent. Add a **startup
validation pass** in `MaiaFactory::new` (and/or a generic `Agent`/`ToolRegistry`
self-check) that logs a `WARN` for each detected misconfig, so the operator
sees them in the boot log instead of as a failed tool call.

Checks to emit at startup:
- **Async sub-agent without a `BackgroundManager`** → the exact `wikis_expert`
  failure (BUG-002). If any registered tool is a `BackgroundTool` and no
  `BackgroundManager` is in runtime → `WARN: async tool 'X' registered but no
  BackgroundManager installed — calls will fail`.
- **Sub-agent `allowed-tools` that don't resolve** in the parent pool (already
  warned per-tool in `register_subagents`; keep).
- **`provider:` key in an AGENT.md not configured** in the registry (env key
  missing) → already warned by `resolve_or`; surface once at boot too.
- **MCP server failed to connect** (e.g. toolbox at :5050 down) → already
  warned; consider a one-line summary `N/M coral tools available`.
- **Session store is read-only / no persister wired** (BUG-001) → warn that
  runs won't persist.

Goal: a clean boot log doubles as a config health check.

---

## BUG-002 ✅ Async sub-agent fails: "BackgroundManager not in runtime context"

**Severity:** high — async sub-agents unusable. `wikis_expert` (only
`dispatch: async` sub-agent) errors on every invocation.

**Symptom**
```
wikis_expert → {"prompt":"...trèfles..."}
wikis_expert: BackgroundManager not in runtime context   (every retry)
→ Maia: "Je n'arrive pas à accéder à l'expert wikis…"
```

**Root cause**
`dispatch: async` → `AsyncSubagentTool` (a `BackgroundTool`). Background
dispatch needs a `BackgroundManager` in runtime to register the task /
return a `task_id`. `register_subagents` registers it via
`ToolRegistry::register_background` (wrap + insert only); the manager is
auto-installed only by `AgentBuilder::background_tool` / `background_manager`.
`MaiaFactory::build` builds the pool then `.tools(pool)`, so auto-install
never fires → manager absent.

**Fix (chosen)** — in `MaiaFactory::build`, after `.tools(pool)`:
```rust
.background_manager(Arc::new(runic_tool_core::BackgroundManager::new()))
```
Installs the manager + registers `background_status` / `background_cancel` so
Maia can poll the async task. Also add the boot-time warning above.
Alt (deferred): switch `wikis_expert` to `dispatch: sync` for single-turn UX.

**Files:** `runic/src/agent/build.rs`

**Status:** ✅ fixed. `MaiaFactory::build` now calls
`.background_manager(Arc::new(BackgroundManager::new()))` after `.tools(pool)`.
Also added the boot-time guard: `ToolDispatch::is_background()` +
`ToolRegistry::has_background_tool()`, and `AgentBuilder::build()` logs a WARN
if a background tool is registered without a manager. Boot log now shows no
warning (manager present).

---

## BUG-001 ✅ Run path never persists to the session store

**Severity:** high — conversations aren't saved. Postgres `session_events`
empty; `list_threads`/`get_thread`/replay return nothing for today's runs.

**Symptom**
- `SELECT count(*) FROM session_events;` → 0 after live chats.
- Only `~/.runic/sessions/.../events.jsonl` present are from **2026-06-05**
  (old `bind_user_context`/`logging` hooks = pre-cleanse binary).

**Root cause**
`runic-serve`'s `create_and_stream_run` streams `AgentEvent`s to SSE but
never writes them to `session_store` — no `.append`, no `spawn_persister` in
the crate. The store is only read (`list`/`get`/replay) + a live
`subscribe_events` for reconnect. Independent of file vs Postgres.
(`DATABASE_URL` *is* in `core/.env` and loads fine — not an env issue.)

**Fix (proposed)** — in the run task, before `run_streaming_*`:
```rust
let rx = agent.subscribe_events();                 // subscribe BEFORE running
runic_sessions::spawn_persister(session_store.clone(), tenant, thread_id, rx);
```
`spawn_persister` (already in `runic-sessions`) appends every `SessionEvent`
to the store keyed by `(tenant, thread)`.

**Files:** `crates/runic-serve/src/pool.rs` (persister spawned at agent
build), `app.rs` (pass `session_store` into `ThreadPool::new`).

**Status:** ✅ fixed. The persister is spawned **once per agent** in
`ThreadPool::get_or_build` (subscribes to the agent's broadcast before the
first run) — NOT per run, which would double-write. Verified live: a single
chat persisted 9 rows to Postgres for `(bugtest, t-persist)` —
`RunStart`, 2× `Message`, `HookRan×4`, `TurnBoundary`, `RunEnd`.

---

## GAP-001 🔵 Threads are scoped per tenant, not per user

**What:** `ThreadPool` keys on `(tenant, thread_id)`; `SessionStore` keys on
`tenant` (first arg). `build_run_context` sets `org_id = tenant`. So
`list_threads` returns every thread in the org; `user_id` (per-run context)
does NOT scope thread ownership.

**Options / suggestion**
1. **tenant = user_id** → per-user isolation for free (set `X-Runic-Tenant`
   to the user id). Note: `org_id` then collapses to user — wrong for
   org-shared data. Also default `user_id ← tenant` in `build_run_context`.
2. **owner-on-thread (Aegra model, recommended w/ auth):** keep `tenant =
   org_id`, store `owner = user_id` at thread creation, filter
   `list`/`get`/`delete` by owner. Needs a thread-metadata slot (the
   event-log store has none today).

**Status:** 🔵 decision pending (depends on auth direction).

---

## GAP-002 ✅ Maia has no memory (no memory tools / mount)

**What:** Maia only persists the *conversation* (within a thread, once BUG-001
is fixed). No cross-thread "notes about the user" store. Live test: "souviens-
toi que je m'appelle Manu" → Maia: "Je n'ai pas de mémoire persistante."

**Suggestion** — runic already ships the subsystem; just wire it in
`MaiaFactory::build`:
- give Maia a **persistent** backend (currently `parent_storage` is an
  ephemeral `MemoryBackend`) → `LocalFsBackend` rooted per scope,
- register `runic_memory::MemoryTool` (writes `memory/MEMORY.md` +
  `memory/USER.md` via `BoundedMemoryStore`),
- add `MemoryLayer` + `UserFactsLayer` to the context engine (inject them).
Scope = same fork as GAP-001 (per-tenant simplest; per-user needs user-keyed
backend since build only knows `(tenant, session)`).

**Status:** ✅ fixed — **per-user**, rooted at `runic-data/{user_id}`. New
`runic/src/agent/memory.rs`: `UserMemoryTool` (the `memory` tool) and
`UserMemoryLayer` (context layer) both resolve `user_id` from the per-run
context at call time and route to a `RootedBackend` at `runic-data/{user_id}`
(reusing `runic_memory::{MemoryTool, BoundedMemoryStore}` +
`runic_context_engine::{MemoryLayer, UserFactsLayer}`). Wired in
`MaiaFactory`. Files: `runic-data/{user_id}/memory/MEMORY.md` + `USER.md`.
No `user_id` in context → the tool errors (won't persist anonymously) and the
layer renders nothing. Unit-tested (persist + per-user isolation).

---

## GAP-003 ⚪ Auth layer (server-injected identity) not built

**What:** `user_id`/`org_id` are currently **client-supplied** (request body
`context` + `X-Runic-Tenant` header) — the "trust the header behind a gateway"
mode. No token verification.

**Suggestion** — add an `AuthUser` extractor in `runic-serve` that validates a
token → verified `{user_id, org_id, claims}`, and in the run handler stamp
those onto `run_ctx.config` **after** `build_run_context` so server values
override client ones (Aegra's `inject_user_context`). No agent-core change —
identity just stops being client-trusted. Pairs with GAP-001 option 2.

**Status:** ⚪ deferred / future.

---

## LIM-001 ⚪ `wikis_expert` on Mistral can't show images

**What:** `get_image` returns a `ToolResultImage`, but the OpenAI-compatible
adapter (Mistral) doesn't encode tool-result images — only the Anthropic +
Gemini adapters do. `ephy_expert` unaffected (no images).

**Suggestion** — add tool-result image encoding to the OpenAI adapter (mirrors
the Anthropic/Gemini work), OR keep image-heavy wikis on a multimodal
provider. `get_page_content` (text) works on Mistral regardless.

**Status:** ⚪ deferred.

---

## LIM-002 ⚪ `search_product_catalog` (ephy RAG tool) not built

**What:** `ephy_expert`'s prompt references a Vertex vector-search catalogue
tool that doesn't exist in runic yet. So Pattern-A "recommend a product"
queries can't be served; the 12 E-Phy MCP tools work.

**Status:** ⚪ deferred (owner dropped RAG for now).

---

## Async UX ✅ non-blocking completion via reminder (not polling)

**What:** async sub-agents (e.g. `wikis_expert`) return a `task_id`; the model
was just sitting and polling `background_status`, defeating the non-blocking
point.

**Fix:**
- Wired `ReminderEngine` + `BackgroundTaskReminder` into Maia's context
  engine, sharing ONE `BackgroundManager` with the async tools. When a task
  finishes, the reminder injects an ambient note (*"task <id> (<tool>)
  completed — <result>"*, deduped) into the prompt on a later turn — the
  agent gets the result pushed to it, no polling.
- Rewrote the guidance text on async/background tools (the sub-agent
  description prefix, the post-call message, and `background_status`'s
  description) to say: fire-and-continue, don't poll in a loop.

**Files:** `runic/src/agent/build.rs` (reminder wiring),
`crates/runic-agents/src/lib.rs` (`augment_async_description`),
`crates/runic-tool-core/src/background.rs` (return msg + status description).

**Status:** ✅ done.

---

## Resolved this session ✅

- ✅ **Per-run context channel** (`RunContext` = open config map + provider
  override) — `run_with` / `run_streaming_message_with`; config set per run,
  restored; provider swap+restore. Tools read `ctx.config`, hooks
  `state.config`, layers `TurnContext.config`.
- ✅ **Tool-level interceptors** (`ToolInterceptor` + `InterceptedTool` +
  `ToolRegistry::intercept`) — binding/guard ride *with the tool*, fire for
  any agent (parent or sub-agent).
- ✅ **Sub-agent inherits parent run context** — `run_subagent` passes
  `ctx.config` down via `child.run_with`; toolbox calls inside `crm_expert`
  etc. get `user_id`/`org_id` stamped; model never sees the keys.
- ✅ **Per-request provider override** — `context.provider` swaps Maia's model
  for that run (verified live on Mistral).
- ✅ **DB session store wired** (Postgres via `DATABASE_URL`, else file) —
  boots + creates schema. (But see BUG-001: nothing is written yet.)
- ✅ **ephy/wikis sub-agents moved gemini → mistral** (key present in env).
- ✅ **UI sends per-run `context`** — free-form JSON box in `runic-web`.

---

## Non-bugs (expected behavior)

- **"coral toolset tool missing on MCP server"** → toolbox at `:5050` not
  running. Start it; warnings clear.
- **Web search "listed but blocked"** → `WebSearchGuard` is fail-closed; tools
  stay visible (model-aware) but the call is denied unless
  `context.allow_web_search = true`. Working as designed. (Optional future:
  hide the tool defs when web is off — needs per-run tool filtering.)
- **Provider override only swaps the main agent**, not sub-agents → by design;
  sub-agents run on their own `AGENT.md` provider.
