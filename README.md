# runic

A personal Rust agent harness. Inspired by the simplicity of pi and the
speed and type-safety of Rust.

Built as a library first — no TUI, no CLI, no UI. You compose an `Agent`
from a provider, tools, hooks, and a context engine, then drop it into
your own surface. The `runic` binary is one such surface: a reference
**HTTP server** (the coral "Maia" agent) that streams runs over SSE and is
driven by the `runic-web` Leptos dev console.

```
$ cargo run -p runic
  ╭─ runic serving (Maia) on http://127.0.0.1:8920
  │  health:  curl -s http://127.0.0.1:8920/healthz
  │  chat (SSE stream): POST /threads/{id}/runs/stream
  ╰─ Ctrl-C to stop.
```

The fastest way to see the library itself is an example:
`ANTHROPIC_API_KEY=sk-... cargo run --example minimal -- "hi there"`.

## What you get

| Feature | Status |
|---|---|
| Streaming providers — Anthropic, Gemini, OpenAI-compatible (Mistral/OpenAI/Groq/local) | ✅ |
| Tool calls (plain, HITL-gated, background) | ✅ |
| Async subagents (sync + background variants) | ✅ |
| Six-point hook system (`before/after` × agent/model/tool) | ✅ |
| Per-run context (`RunContext` — open config map + provider override) | ✅ |
| Tool interceptors (guards/bindings that ride with the tool to sub-agents) | ✅ |
| Per-run, per-tool call limits (`CallLimitHook`) | ✅ |
| Pluggable storage backends (FS, in-memory, overlay, namespaced) | ✅ |
| Composable context engine (layers + decorator engines) | ✅ |
| Skills (Claude Code-compatible `SKILL.md` files) | ✅ |
| Markdown sub-agents (`AGENT.md`) | ✅ |
| Plugin bundles (`plugins/{name}/{skills,agents}/`) | ✅ |
| MCP client (stdio + Streamable HTTP) | ✅ |
| Spillover (huge tool outputs → disk) | ✅ |
| Compactor (summarize old messages when context fills) | ✅ |
| Reminder (peripheral vision via pluggable sources) | ✅ |
| Persistence (pluggable `SessionStore` — file or Postgres, multi-tenant) | ✅ |
| HTTP server (threads, SSE runs, resume, replay-on-restart) + Leptos dev console | ✅ |

## 60-second quickstart

**As a library** (the smallest possible agent):
```sh
export ANTHROPIC_API_KEY=sk-...
cargo run --example minimal -- "what's the capital of France?"
```
Then read `crates/runic-examples/` for tools, hooks, per-run context,
interceptors, and call limits. To build your own agent, see
[docs/extending.md](./docs/extending.md).

**As a server** (the reference coral "Maia" agent + web console):
```sh
# 1. terminal one — the HTTP server (binds 127.0.0.1:8920)
export ANTHROPIC_API_KEY=sk-...        # + GEMINI/MISTRAL keys for sub-agents
# optional: export DATABASE_URL=postgres://…   (else a file store is used)
cargo run -p runic

# 2. terminal two — the Leptos dev console at http://127.0.0.1:8080
cd crates/runic-web && trunk serve --port 8080
```
The console drives the server over HTTP+SSE: thread list, streaming chat
with tool-call cards, a run/turn-clustered event inspector, and a
"Configurable" panel for per-run context (`user_id`, `provider`, …).

## Documentation

| Doc | What it covers |
|---|---|
| [ARCHITECTURE.md](./ARCHITECTURE.md) | Design overview, crate map, request flow |
| [docs/skills.md](./docs/skills.md) | Writing skills (`SKILL.md`) |
| [docs/agents.md](./docs/agents.md) | Writing markdown sub-agents (`AGENT.md`) |
| [docs/plugins.md](./docs/plugins.md) | Bundling skills + agents as plugins |
| [docs/mcp.md](./docs/mcp.md) | Configuring MCP servers (stdio + HTTP) |
| [docs/context-engine.md](./docs/context-engine.md) | The context pipeline (layers, compactor, spillover, reminder) |
| [docs/extending.md](./docs/extending.md) | Writing your own Tool / Hook / Reminder / ContextLayer |
| [docs/persistence.md](./docs/persistence.md) | Multi-tenant session persistence; pluggable `SessionStore`; replay |
| [docs/blobs.md](./docs/blobs.md) | Content-addressed blob storage; provider materialization |

## Runnable examples

```sh
cargo run --example minimal             # ~30-line agent loop
cargo run --example with_tools          # custom Tool impl
cargo run --example with_hooks          # custom Hook impl
cargo run --example with_run_context    # per-run RunContext (config + provider override)
cargo run --example with_interceptor    # ToolInterceptor that stamps per-run identity
cargo run --example with_call_limit     # CallLimitHook caps per-tool calls per run
cargo run --example custom_reminder     # write your own Reminder
cargo run --example with_mcp            # connect to a local MCP server
cargo run --example with_blob -- IMG    # upload a file as a blob + ask the model about it
```

Each example is self-contained and commented. See `crates/runic-examples/`.

## Crate map (22 crates)

```
runic-message-types       wire types (Message, ContentBlock, StreamEvent, ToolCall)
runic-provider-core       Provider trait + retry policy
runic-provider-anthropic  Anthropic SSE client
runic-provider-gemini     Gemini client
runic-provider-openai     OpenAI-compatible client (Mistral, OpenAI, Groq, local, …)
runic-storage-backend     StorageBackend trait + LocalFs, Memory, Overlay, Namespaced impls
runic-tool-core           Tool / HitlTool / BackgroundTool + dispatch registry
runic-context-engine      ContextEngine trait + Composite + Compactor + Spillover + Reminder
runic-agent-core          Agent loop, hooks, state, sub-agent dispatch
runic-skills              SKILL.md parser, registry, layer, view tool
runic-commands            COMMAND.md parser + registry (slash-command prompt templates)
runic-agents              AGENT.md parser, registry, conversion to SubagentTool
runic-plugins             ~/.runic/plugins/{name}/ discovery, aggregate registries
runic-mcp                 MCP client (stdio + Streamable HTTP transports)
runic-sessions            SessionStore trait + File + Postgres stores + spawn_persister + replay
runic-blobs               BlobStore trait + FileBlobStore (sha256, dedup) + materializing provider
runic-memory              bounded MEMORY.md / USER.md stores + memory tool (threat-scanned)
runic-shell-tools         read/write/edit/ls/glob/grep tools over any StorageBackend
runic-serve               axum HTTP server — threads, runs, SSE streaming, resume, HITL
runic-web                 Leptos dev console (WASM) for the runic HTTP server
runic                     binary: reference HTTP server (the coral "Maia" agent)
runic-examples            runnable examples
```

The dependency DAG is documented in [ARCHITECTURE.md](./ARCHITECTURE.md#crate-dependency-graph).

## Environment knobs

| Variable | Purpose | Default |
|---|---|---|
| `RUNIC_HOME` | Root for skills, agents, plugins, mcp.json, memory | `~/.runic` |
| `RUNIC_PROVIDER` | `anthropic`, `gemini`, `mistral`, `openai`, or `openai-compatible` | `anthropic` |
| `RUNIC_MODEL` | Provider model override | provider default |
| `ANTHROPIC_API_KEY` | Required when provider is Anthropic | — |
| `GEMINI_API_KEY` | Required when provider is Gemini | — |
| `MISTRAL_API_KEY` | Required when provider is Mistral | — |
| `OPENAI_API_KEY` | Required when provider is OpenAI / openai-compatible | — |
| `RUNIC_OPENAI_BASE_URL` | Endpoint for `openai-compatible` (Groq, OpenRouter, local, …) | — |
| `RUNIC_SPILLOVER_THRESHOLD` | Bytes above which a tool result gets spilled | `8192` |
| `RUNIC_COMPACT_THRESHOLD` | Token count above which to compact | `100000` |
| `RUNIC_SPILLOVER_RETENTION_DAYS` | Spillover files older than this are deleted at startup (`0` disables) | `14` |
| `DATABASE_URL` | Postgres connection string; when set, runs persist to Postgres (else a file store under `~/.runic/sessions`) | unset |
| `RUNIC_DATA_DIR` | Root for per-user memory (`{dir}/{user_id}/memory/…`) | `<workspace>/runic-data` |
| `RUNIC_ADDR` | Bind address for the HTTP server binary | `127.0.0.1:8920` |

## Serving over HTTP

```sh
cargo run -p runic                  # binds 127.0.0.1:8920 (override with RUNIC_ADDR)
```

The server exposes threads (== sessions) and SSE-streamed runs; the
tenant comes from the `X-Runic-Tenant` header (defaults to `default`).
Per-run context (identity, web opt-in, a provider override) rides in the
request body's `context` object — see [docs/extending.md](./docs/extending.md#per-run-context-runcontext).

```sh
curl -XPOST localhost:8920/threads -H 'x-runic-tenant: alice' -d '{"thread_id":"t1"}'

# Stream a run. `context` is an open map the server interprets (model never sees it):
curl -N -XPOST localhost:8920/threads/t1/runs/stream -H 'x-runic-tenant: alice' \
  -d '{"message":"hi","context":{"user_id":"u1","provider":"haiku"}}'

# Resume a run from where a dropped connection left off:
curl -N localhost:8920/threads/t1/runs/<run_id>/stream -H 'last-event-id: 42'
```

Runs persist automatically (Postgres when `DATABASE_URL` is set, else a
file store). A thread that goes cold (server restart, eviction) is rebuilt
from its persisted events on the next request — full history intact. A
client disconnect mid-run never bricks the thread: the run finishes
server-side and the agent returns to the pool. The `runic-web` Leptos
console (`cd crates/runic-web && trunk serve`) is a richer alternative to
curl.

## What's not built yet

- Serve mode hardening (auth, persistent run queue, pool eviction, run cancellation)
- MemoryStore (cross-session memory; separate from `SessionStore`)

See the roadmap section of [ARCHITECTURE.md](./ARCHITECTURE.md#whats-not-built).
