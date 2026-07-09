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
use std::sync::Arc;
use runic_agent::Agent;
use runic_provider::{Provider, mistral::MistralDriver};
use runic_tools::default_tools;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key = std::env::var("MISTRAL_API_KEY")?;
    let provider: Arc<dyn Provider> = Arc::new(MistralDriver::new(key));

    let mut b = Agent::builder(provider, "user", "scratch").model("mistral-medium-latest");
    for t in default_tools() {
        b = b.tool(t);
    }
    let mut agent = b.build();

    let outcome = agent.run("What is 17 * 23? Use the calculator.").await?;
    println!("{}", agent.state().last_assistant_text().unwrap_or_default());
    println!("turns: {}  usage: {:?}", outcome.total_turns, outcome.usage);
    Ok(())
}
```

## What's in the box

| Area | |
|---|---|
| **Providers** | Anthropic, Gemini, native **Mistral** (document PDFs, thinking on/off/effort), and OpenAI-compatible (OpenAI / Groq / local) behind one `Provider` trait, streaming + non-streaming |
| **Event-sourced state** | append-only `SessionEvent` log → `messages_for_provider()`; compaction is a non-destructive snapshot |
| **Tools** | `calculator`, `system_time`, `web_fetch`, `web_search`, `weather` + `weather_history`, `composio`, `ask_user` / `escalate_to_human`, `read_thread_artifact`, `memory`, `search_chats`, `skill_view`, `delegate`, `tool_search` |
| **Resilient dispatch** | per-tool timeout, panic isolation (a buggy tool becomes an error result, never aborts the run), parallel + serial batches |
| **Hooks** | six points (`before/after` × agent/model/tool), read (parallel) + write (sequential), plus a loop guard |
| **Structured output** | `AgentBuilder::output_schema(schema)` — provider-agnostic, via a synthetic `final_answer` tool → `RunOutcome.structured` |
| **Memory** | bounded `MEMORY.md` / `USER.md` stores + a `memory` tool, provider/manager seam (hermes-style) |
| **Subagents** | a single `delegate` tool over an `AGENT.md` roster |
| **Skills / commands / plugins** | `SKILL.md` (progressive disclosure), `COMMAND.md` templates, folder-bundle plugins |
| **MCP** | client over stdio + Streamable HTTP, with reconnect, deferred activation, and `tool_search` |
| **Persistence** | `runic-substrate`: Postgres / in-memory session stores, artifacts, event-sourced, full-text `search_chats` |
| **Durable suspend/resume** | a tool can defer (`ToolResult::defer`); the run pauses durably and resumes from that exact call on any instance once an answer arrives — HITL (`ask_user`) is the first consumer |
| **HTTP** | `runic-serve` (axum: threads, SSE runs, pooling, resume/replay, deferred-tool answers) |

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
runic-tools       the native toolbox (calc, time, web, weather, composio, hitl)
runic-skills      SKILL.md registry + skill_view tool
runic-commands    COMMAND.md slash-command templates
runic-mcp         MCP client (stdio + Streamable HTTP)
runic-plugins     folder-bundle plugin discovery
runic-substrate   sessions + artifacts persistence (Postgres / local / memory) + search_chats
runic-memory      bounded MEMORY.md / USER.md stores + memory tool + providers
runic-transcriber speech-to-text trait + Mistral/Voxtral (audio → text preprocess)
runic-serve       axum HTTP server (threads, SSE runs, pooling, resume, deferred-tool answers)
runic             umbrella crate — re-exports the whole SDK behind one dependency
```

## Developing

```sh
cargo test --workspace        # unit + property tests (proptest across 6 crates)
cargo clippy --workspace --all-targets -- -D warnings
lefthook install              # once per clone — pre-commit runs fmt + clippy
```

Property tests cover the harness's load-bearing invariants — event-sourced
replay, the session store's monotonic seq, the bounded memory cap, the wire
mapping, and message round-trips. `crates/runic-tools/fuzz/` holds a cargo-fuzz
scaffold for the text parsers.

## Status

A personal project, built by synthesizing ideas from a few reference harnesses
into its own Rust-idiomatic design. The core (loop, tools, providers, hooks,
memory, subagents, MCP, persistence, server), the composable `Agent` assembly
layer, durable suspend/resume, and first-class Mistral (document PDFs, thinking
control) are in place. Deferred: broader multimodal, background memory review,
deeper observability.
