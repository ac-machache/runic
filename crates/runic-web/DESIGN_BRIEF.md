# Claude Design brief — runic dev console

> Paste this whole document into Claude Design. It builds the visual design +
> standalone HTML/CSS; we port it into a Leptos (Rust/WASM) app afterward.

---

## 1. What we're designing

**runic dev console** — a local developer UI for driving an AI agent server
("runic serve"). It is *not* a consumer chatbot; it's an operator/debugging
console for an engineer building and inspecting an agent. Think "Postman +
chat + a tracing inspector" in one screen. Single user, runs on localhost,
talks to the server over HTTP + Server-Sent-Events.

The whole app is one screen, three regions:
1. **Left sidebar** — connection settings + thread list (collapsible).
2. **Chat** — the conversation with the agent (streaming).
3. **Inspector** — a debugging panel with two tabs: **Events** and **State**.

Chat and Inspector sit **side by side at 50/50 width** (resizable divider is a
plus). The sidebar can collapse to give chat+inspector the full width.

---

## 2. Design language (important — follow this, don't default to a generic SaaS look)

Use **Anthropic's / Claude's own brand aesthetic**: warm, calm, paper-like,
thoughtfully restrained — the opposite of a cold neon "developer terminal."

- **Palette (light, default):** warm off-white "paper" background (#F7F3EC /
  #FAF7F1), near-black ink text (#191919), warm grey secondary text. Primary
  accent = Claude clay/rust-orange **#C15F3C** ("Crail"), with a softer tan
  **#D4A27F** as a secondary. Success = a muted moss green, error = a warm
  brick red, warning = a warm amber. **No cold blues, no neon.**
- **Palette (dark variant):** a *warm* dark — soft charcoal/espresso (#1C1A17
  / #232019), warm off-white text, the same clay accent. Not cold navy.
  Provide both themes with a toggle.
- **Typography:** geometric, slightly characterful sans for UI and headings
  (Styrene-like — fall back to Inter/system if unavailable); a humanist
  **monospace** for code, JSON, IDs, tool args, and event payloads
  (Berkeley/JetBrains/Söhne-Mono-like). A Tiempos-like serif is welcome for
  large empty-state headlines only — keep body UI in sans.
- **Feel:** generous whitespace, soft radii (10–14px), hairline warm borders,
  very subtle shadows for elevation (cards/composer), restrained micro-motion
  (120–180ms ease). Density should be *comfortable*, not cramped — but this is
  a power tool, so don't waste vertical space either.

---

## 3. Layout & chrome

```
┌──────────────┬───────────────────────────┬───────────────────────────┐
│  SIDEBAR     │   CHAT  (50%)             │   INSPECTOR (50%)         │
│ (collapsible)│                           │   [ Events | State ]      │
│              │   transcript (scrolls)    │                           │
│  connection  │                           │   tab content (scrolls)   │
│  threads     │                           │                           │
│              │   ── composer ──          │                           │
│              │   [textarea] [⚙][Send]    │                           │
└──────────────┴───────────────────────────┴───────────────────────────┘
```

- **Sidebar collapse:** a toggle (hamburger / chevron) collapses the sidebar
  entirely (or to a thin icon rail). When collapsed, Chat + Inspector expand to
  fill. Show a thin "open sidebar" affordance when collapsed.
- **Chat ↔ Inspector are 50/50** by default, divided by a draggable vertical
  splitter (snap back to 50/50 on double-click).
- Slim top bar per region with its title; a global streaming indicator when a
  run is active.

---

## 4. Components — detailed requirements

### 4.1 Sidebar (collapsible)
- **Brand**: a small "⟡ runic" wordmark + "dev console" subtitle.
- **Connection** group: `server URL` (default `http://127.0.0.1:8920`) and
  `tenant` (default `default`) — small labeled mono inputs. (These used to be
  cramped at the top; give them a tidy, clearly-grouped block.)
- **Threads**: a "＋ New thread" button, a "⟳ refresh" icon, and a list of
  threads. Each thread today is only a short id (e.g. `t-9f2a…`) — render it as
  a clean list row with hover + an **active** state (clay left-accent bar).
  Design the row so it can later also show a **title** and a relative timestamp
  (leave room; title may be empty for now).
- Empty state when there are no threads.

### 4.2 Chat transcript
Render, top to bottom, a vertical conversation. Message kinds:
- **User message** — right-aligned, in a warm clay/tan bubble, white-ish text.
- **Assistant message** — left-aligned, *no bubble* (open on the paper), full
  **Markdown** (headings, lists, code blocks, tables, links, blockquote).
  A small assistant avatar/glyph to its left.
- **Thinking** (optional reasoning) — a collapsed, muted, italic block labeled
  "thinking", expandable. Off by default.
- **Tool call card** — see 4.3 (this needs a real redesign).
- **Warning** — a warm amber inline notice.
- **Streaming tail** — while tokens arrive, show the in-progress assistant text
  (and a subtle caret/typing indicator).
- **DO NOT render a "structured output" panel.** (We are removing it entirely.)
- Empty states: "Create or select a thread", and "Send a message to start".

### 4.3 Tool-call card (in the chat) — redesign this, the current one is weak
A compact but informative card that reads at a glance and expands for detail:
- **Header row:** a status glyph + the **tool name** (mono), a status pill
  (`running` / `done` / `error`, color-coded warm), and a right-aligned
  **duration** (e.g. `· 840ms`).
- **Args** (collapsible): the tool input as pretty-printed JSON in a mono block.
  Collapsed by default if large.
- **Result** (collapsible): a trimmed preview of the tool result (mono);
  error results tinted with the warm-red treatment.
- **Grounding sources** (when present): a row of pill "chips" linking out
  (title + 🔗), from the tool's `metadata.sources`.
- Make sub-agent/`mcp__…` tools visually legible (e.g. show a cleaned label
  like `tavily · search` from `mcp__tavily__tavily_search`).
- Tool cards should feel **secondary** to the conversation (indented / lower
  contrast than messages), not shouting.

### 4.4 Composer (bottom of chat)
- A **multiline auto-growing textarea** (1 line → grows to ~6, then scrolls).
  **Enter = send, Shift+Enter = newline.** Placeholder adapts (no thread vs
  ready).
- A **Send** button (clay). While a run streams, it becomes a **Stop** button.
- A **"⚙ Configurable" button** next to Send (this is the key change). Clicking
  it opens a **popover / slide-up sheet** — modeled on LangGraph Studio's
  "Configurable" panel — holding the **per-run context**, so it's *out of the
  sidebar* and attached to the input where it belongs. Fields:
  - `user_id` (text)
  - `org_id` (text)
  - `provider` (select: `haiku` / `sonnet` / `gemini` / `mistral`, or blank =
    server default)
  - `allow_web_search` (toggle)
  - an "advanced" escape hatch: a raw **JSON** editor for arbitrary extra
    context keys (the context is an open map).
  - (separately, an optional **output schema** JSON field — keep it here too,
    or in its own small disclosure.)
  - Show a small dot/badge on the ⚙ button when any context is set.
- A subtle hint line: "Enter to send · Shift+Enter for newline".

### 4.5 Inspector — Tab A: **Events** (this is the most important redesign)
Today this is a raw firehose: one row **per streamed token**, reverse-ordered,
each showing truncated JSON. **Replace it with a clustered, hierarchical tree.**

Cluster the event stream into **Run → Turn → details**, newest run on top,
**collapsed by default**:

- **Run** (top level, collapsible): header shows a short run id, a relative
  time, **#turns**, the final **stop_reason**, and **token usage**
  (`in / out`, plus cache reads if present). A status dot (running / done /
  error).
  - **Turn** (nested, collapsible): "Turn 1", "Turn 2"… For each turn show:
    - **Assistant text** for that turn — **coalesced into one block** (NOT one
      row per token). Markdown or plain is fine; mono ok.
    - **Thinking** (if any) — collapsed.
    - **Tool calls** in that turn, each: tool name, **args** (pretty JSON),
      and **result** (preview, error flag, duration). Same expandable feel as
      the chat tool card but denser.
    - A turn footer: `stop_reason`, `tool_calls_this_turn`.
- Provide a small filter/toggle row (e.g. show/hide thinking; collapse all /
  expand all).
- It must stay readable during a *live* run (new turns append, the open run
  grows) — design the streaming/loading state of this tree.

This clustering is the #1 ask. No per-token rows. Ever.

### 4.6 Inspector — Tab B: **State** (present this much better than today)
The server returns the agent's full inspectable state. Today it's a wall of
collapsibles. Design a clean, scannable layout:
- A **header strip** of counts: `N runs · M events · K messages`, plus a `busy`
  badge when a run is in flight.
- **System prompt**: a clean reader for the **assembled** prompt (what the
  model actually sees) with a toggle to view the **base** prompt. Monospace,
  scrollable, with a copy button. (These can be long.)
- **Tools** (`tools[]`): a searchable list; each tool row shows `name` +
  one-line `description`, expandable to its `input_schema` (pretty JSON).
  Show the count.
- **Messages** (`messages[]`, exactly as sent to the model): a compact
  timeline; each item shows a role chip (user/assistant/tool) and a one-line
  summary of its content blocks (text / `tool_use name(args…)` /
  `tool_result …` / `[image]` / `[thinking]`), expandable to full.
- A refresh control.

---

## 5. Real data contract (use these shapes for realistic mock content)

All requests carry header `X-Runic-Tenant: <tenant>`. Base URL is the
`server` field (default `http://127.0.0.1:8920`).

**Endpoints**
- `GET  /threads` → `{ "threads": [ { "thread_id": "t-…" } ] }`
- `POST /threads` `{ "thread_id"?: "…" }` → `{ "thread_id": "…" }`
- `GET  /threads/{id}/events` → `{ "events": [ { "seq": n, "event": <SessionEvent> } ] }`
- `GET  /threads/{id}/state` → state object (below)
- `POST /threads/{id}/runs/stream` body `{ "message": "...", "context"?: {...}, "output_schema"?: {...} }` → **SSE** stream of WireEvents
- `POST /threads/{id}/runs/live/approvals/{call_id}` body `{ "decision":"submit","final_input":{…} }` or `{ "decision":"cancel","reason":"…" }`

**SSE event types** (each frame is `data: {"type":"…", …}`):
- `run_start` `{ run_id, at }`
- `assistant_text_delta` `{ text }`   ← streamed per token (cluster/coalesce these)
- `assistant_thinking_delta` `{ text }`
- `tool_start` `{ id, name }`
- `tool_dispatching` `{ id, name, input }`            ← input = the tool args
- `tool_finish` `{ id, name, is_error, duration_ms, preview, metadata? }`
  - `metadata.sources` = `[ { "title": "...", "url": "..." } ]` (grounding chips)
- `message` `{ run_id, msg: <Message>, at }`
- `turn_complete` `{ stop_reason, tool_calls_this_turn }`
- `run_end` `{ run_id, total_turns, stop_reason, at }`
- `usage` `{ input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens }`
- `warning` `{ message }`
- `approval_required` `{ call_id, tool_name, draft: { summary, current_input, input_schema, editable_fields } }`
- `structured_output` `{ result }`   ← **ignore in the UI (do not render)**
- `done` `{ total_turns }`

**Message** = `{ "role": "user"|"assistant", "content": [ ContentBlock ], "timestamp" }`
**ContentBlock** one of:
- `{ "type":"text", "text":"…" }`
- `{ "type":"reasoning", "text":"…" }`
- `{ "type":"tool_use", "id":"…", "name":"…", "input": {…} }`
- `{ "type":"tool_result", "tool_use_id":"…", "content":"…", "is_error":bool, "metadata"?, "images"? }`
- `{ "type":"image", "media_type":"…", "data":"…" }`

**Persisted SessionEvent** (from `/events`, has a `kind`, *not* `type`):
`RunStart{run_id,at}`, `RunEnd{run_id,outcome,at}`, `Message{run_id,msg,at}`,
`TurnBoundary{run_id,at}`, `HookRan{run_id,hook,lifecycle,at}`,
`StateSnapshot{run_id,messages,…}` (a compaction; replaces prior history).
The Events tab is built by clustering these (or the live SSE) by `run_id`,
then by turn (a turn boundary = `turn_complete` / `TurnBoundary`).

**State object** (`/state`):
```json
{
  "thread_id": "t-…", "tenant": "default", "busy": false,
  "base_system_prompt": "…", "assembled_system_prompt": "…",
  "tools": [ { "name": "…", "description": "…", "input_schema": {…} } ],
  "messages": [ <Message> ],
  "event_count": 42, "run_count": 3
}
```
(When `busy:true`, prompt/tools are empty and messages come from the store.)

---

## 6. States to design (please cover all)
- Sidebar: expanded, collapsed, empty thread list, active thread.
- Chat: empty (no thread / no messages), streaming (live tail), tool running →
  done → error, approval-required card (a parked HITL tool with an editable
  form built from `draft.editable_fields`), warning, long markdown answer.
- Composer: idle, disabled (no thread), streaming (Stop), ⚙ popover open with
  context set (badge on), invalid-JSON in the advanced/schema box.
- Events tab: empty, a finished multi-run/multi-turn tree (collapsed +
  expanded), a live run growing.
- State tab: loaded, `busy`, long prompt, many tools, many messages.
- Light theme and dark theme.

---

## 7. Output we want from you (Claude Design)
- A polished visual design realized as **standalone, self-contained HTML + CSS**
  (no required JS framework — we re-implement behavior in Leptos). 
- A documented **design-token system** as CSS custom properties (colors, type
  scale, spacing, radii, shadows) for **both light and dark**.
- **Component-level markup + CSS** for every component and state in §4–§6, using
  the **real mock data** from §5 so it looks true to life.
- Keep the DOM **semantic and class-driven** (clean class names) so it ports
  cleanly to a Leptos `view!` template + a single `style.css`.
- A short **handoff note** mapping each component to its class names.
- A **Claude Code handoff bundle** is welcome.

## 8. Don'ts
- ❌ No per-token event rows / raw firehose. Cluster Run → Turn → details.
- ❌ No "structured output" panel anywhere.
- ❌ Don't bury per-run context in the sidebar — it lives behind the composer's
  ⚙ Configurable button.
- ❌ No cold blue / neon "hacker terminal" palette — warm Claude aesthetic.
- ❌ Don't make tool cards louder than the conversation.
- ❌ Don't assume React/Vue; deliver portable HTML/CSS.
```
