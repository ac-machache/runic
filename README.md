<p align="center">
  <img src="runic-logo.png" alt="runic" width="540" />
</p>

<p align="center"><em>A personal, library-first agent harness in Rust.</em></p>

---

runic is a workspace of small, composable crates: you build an `Agent` from a
provider, some tools, and optional hooks, then run it — directly, behind an HTTP
server, or inside your own surface. State is **event-sourced** (the provider's
message list is *derived* from an append-only log), tools are panic-isolated,
and the whole thing is sync-free where it counts and `cargo test`-fast.

`runic-serve` is a batteries-included HTTP surface — Postgres-persisted threads,
SSE runs, pooling, and durable resume — that you mount in your own binary,
supplying the provider, tools, and stores.

## A minimal agent

```rust
use runic::Compose;
use runic::ability::*;
use runic_agent::Agent;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut agent = Agent::compose("mistral:mistral-medium-latest")?
        .instructions("You are a helpful assistant.")
        .with(basics())
        .build("user", "scratch")
        .await?;

    let outcome = agent.run("What is 17 * 23? Use the calculator.").await?;
    println!("{}", agent.state().last_assistant_text().unwrap_or_default());
    println!("turns: {}  usage: {:?}", outcome.total_turns, outcome.usage);
    Ok(())
}
```

`Agent::compose("provider:model")` infers the driver and reads its API key from
the environment (`MISTRAL_API_KEY`, etc.); `Composer::new(provider, model)` is
the escape hatch for a custom or already-constructed `Provider`.

## What's in the box

| Area | |
|---|---|
| **Providers** | Anthropic, Gemini, native **Mistral** (document PDFs, thinking on/off/effort), and OpenAI-compatible (OpenAI / Groq / local) behind one `Provider` trait, streaming + non-streaming |
| **Event-sourced state** | append-only `SessionEvent` log → `messages_for_provider()`; compaction is a non-destructive snapshot |
| **Abilities** | a capability package (prompt + tools + hooks + skills + subagents), composed with `.with(...)`; eager or `.deferred()` with a stable catalog, gated live, loaded on demand via `load_ability`, state-persisted and rebuild/compaction-safe |
| **Built-in tool abilities** | `basics()` (calculator, system_time), `ask_user()`, `web_fetch()`, `web_search(provider)`, `weather()`, `composio(key, entity)` — each opt-in, `.with(web_fetch())` |
| **`#[tool]` macro** | derive a tool's name/description/schema from a plain async fn + doc comment (`runic-macros`) |
| **Typed output** | `Composer::output::<T>()` derives the schema from any `schemars::JsonSchema` type; `outcome.output_as::<T>()` deserializes the result |
| **Agent overview** | `GET /agents/{name}` — an ability-grouped structural view (tools/skills/subagents/hooks per ability) for a UI or a human debugging the agent |
| **Resilient dispatch** | per-tool timeout, panic isolation (a buggy tool becomes an error result, never aborts the run), parallel + serial batches |
| **Hooks** | six points (`before/after` × agent/model/tool), read (parallel) + write (sequential), plus a loop guard |
| **Memory** | bounded `MEMORY.md` / `USER.md` stores + a `memory` tool, provider/manager seam (hermes-style) |
| **Subagents** | a single `delegate` tool over an `AGENT.md` roster |
| **Skills / commands** | `SKILL.md` progressive disclosure and `COMMAND.md` slash-command templates |
| **MCP** | client over stdio + Streamable HTTP, with reconnect, deferred activation, and `tool_search` |
| **Persistence** | `runic-substrate`: Postgres / in-memory session stores, artifacts, event-sourced, full-text `search_chats` |
| **Durable suspend/resume** | a tool can defer (`ToolResult::defer`); the run pauses durably and resumes from that exact call on any instance once an answer arrives — HITL (`ask_user`) is the first consumer |
| **HTTP** | `runic-serve` (axum: threads, SSE runs, pooling, resume/replay, deferred-tool answers, agent overview) |

## Serve it over HTTP

Mount `runic-serve` in your binary — it gives you an axum `Router` via
`runic_serve::router(config)`, or a one-call `runic_serve::serve(config, addr)`,
built from a `ServeConfig` that wires your session + artifact stores and an
`AgentFactory`:

```rust
use runic_serve::{ServeConfig, serve, single_agent};

let config = ServeConfig::new(session_store, artifact_store, single_agent("main", factory));
serve(config, "127.0.0.1:8920").await?;
```

Then the surface is threads + SSE runs:

```sh
curl -XPOST localhost:8920/threads -H 'x-runic-tenant: alice' -d '{"thread_id":"t1"}'
curl -N -XPOST localhost:8920/threads/t1/runs/stream -H 'x-runic-tenant: alice' \
  -d '{"message":"what is the weather in Tokyo?"}'
```

Threads persist to Postgres and rebuild from their event log on the next request
after a restart; a client disconnect mid-run never bricks a thread.

## Crate map

```
runic-types       wire types (Message, ContentBlock, ToolCall, TokenUsage)
runic-state       event-sourced AgentState + SessionEvent log
runic-provider    Provider trait + Anthropic / Gemini / Mistral / OpenAI-compatible drivers
runic-tool        Tool trait, ToolContext, HumanInterface
runic-hook        ReadHook / WriteHook (six lifecycle points)
runic-agent       the agent loop — turns, dispatch, hooks, structured output, suspend/resume
runic-subagent    delegate tool + AGENT.md roster + ChildBuilder
runic-skills      SKILL.md registry + skill_view tool
runic-commands    COMMAND.md slash-command templates
runic-mcp         MCP client (stdio + Streamable HTTP)
runic-substrate   sessions + artifacts persistence (Postgres / local / memory) + search_chats
runic-memory      bounded MEMORY.md / USER.md stores + memory tool + providers
runic-transcriber speech-to-text trait + Mistral/Voxtral (audio → text preprocess)
runic-macros      the #[tool] proc-macro
runic-serve       axum HTTP server (threads, SSE runs, pooling, resume, deferred-tool answers)
runic             umbrella crate — Composer/Ability, the built-in tool abilities (calc, time,
                  web, weather, composio, hitl), typed output; re-exports the whole SDK
```

## Developing

```sh
cargo test --workspace        # unit + property tests (proptest across 6 crates)
cargo clippy --workspace --all-targets -- -D warnings
lefthook install              # once per clone — pre-commit runs fmt + clippy
```

Property tests cover the harness's load-bearing invariants — event-sourced
replay, the session store's monotonic seq, the bounded memory cap, the wire
mapping, message round-trips, ability composition (tool/skill/subagent gating
matches what's actually built), and the run-coordination queue on both the
in-memory and Postgres backends. `e2e/harness/` runs the same invariants
end-to-end over real HTTP, real multi-instance concurrency, and (opt-in) real
Postgres/Redis.

## Status

A personal project, built by synthesizing ideas from a few reference harnesses
into its own Rust-idiomatic design. The core (loop, tools, providers, hooks,
memory, subagents, MCP, persistence, server), the composable ability/`Composer`
layer (eager/deferred activation, live gating, an ability-grouped agent
overview endpoint), durable suspend/resume verified under real multi-instance
concurrency, and first-class Mistral (document PDFs, thinking control) are in
place. Deferred: broader multimodal, background memory review, deeper
observability.
