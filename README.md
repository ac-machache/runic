# runic

A personal Rust agent harness. Inspired by the simplicity of pi and the
speed and type-safety of Rust.

Built as a library first — no TUI, no CLI, no UI. A minimal REPL ships
in the `runic` binary just for testing. Drop the agent into your own
surface (HTTP server, desktop app, custom CLI, whatever).

```
$ ANTHROPIC_API_KEY=sk-... cargo run --bin runic
[runic-home] /Users/you/.runic
[skills] 3 total skill(s) registered: ["code-review", "greeter", "optimize"]
[agents] 1 total markdown agent(s) registered: ["researcher"]
[mcp] connecting to 1 server(s) from /Users/you/.runic/mcp.json
[context] compact_threshold=100000 tokens, spillover_threshold=8192 bytes
> hi
hello! how can I help today?
```

## What you get

| Feature | Status |
|---|---|
| Streaming Anthropic + Gemini providers | ✅ |
| Tool calls (plain, HITL-gated, background) | ✅ |
| Async subagents (sync + background variants) | ✅ |
| Six-point hook system (`before/after` × agent/model/tool) | ✅ |
| Pluggable storage backends (FS, in-memory, overlay, namespaced) | ✅ |
| Composable context engine (layers + decorator engines) | ✅ |
| Skills (Claude Code-compatible `SKILL.md` files) | ✅ |
| Markdown sub-agents (`AGENT.md`) | ✅ |
| Plugin bundles (`plugins/{name}/{skills,agents}/`) | ✅ |
| MCP client (stdio + Streamable HTTP) | ✅ |
| Spillover (huge tool outputs → disk) | ✅ |
| Compactor (summarize old messages when context fills) | ✅ |
| Reminder (peripheral vision via pluggable sources) | ✅ |

## 60-second quickstart

1. **Set an API key.**
   ```sh
   export ANTHROPIC_API_KEY=sk-...
   # or
   export RUNIC_PROVIDER=gemini GEMINI_API_KEY=...
   ```

2. **(Optional) Personalize the agent.** Drop a SOUL.md and a memory file:
   ```sh
   mkdir -p ~/.runic/memory
   echo "You are warm, terse, and very curious." > ~/.runic/SOUL.md
   echo "- machache prefers concise answers" > ~/.runic/memory/USER.md
   ```

3. **(Optional) Ship a skill.**
   ```sh
   mkdir -p ~/.runic/skills/greet
   cat > ~/.runic/skills/greet/SKILL.md <<'EOF'
   ---
   name: greet
   description: Greet the user warmly by name
   ---
   When invoked, respond with a 1-2 sentence personalized greeting.
   EOF
   ```

4. **Run.**
   ```sh
   cargo run --bin runic
   ```
   Type messages. `/state` for a summary, `/dump` for full JSON, `/quit` to exit.

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

## Runnable examples

```sh
cargo run --example minimal             # ~30-line agent loop
cargo run --example with_tools          # custom Tool impl
cargo run --example with_hooks          # custom Hook impl
cargo run --example custom_reminder     # write your own Reminder
cargo run --example with_mcp            # connect to a local MCP server
```

Each example is self-contained and commented. See `crates/runic-examples/`.

## Crate map (14 crates)

```
runic-message-types       wire types (Message, ContentBlock, StreamEvent, ToolCall)
runic-provider-core       Provider trait + retry policy
runic-provider-anthropic  Anthropic SSE client
runic-provider-gemini     Gemini client
runic-storage-backend     StorageBackend trait + LocalFs, Memory, Overlay, Namespaced impls
runic-tool-core           Tool / HitlTool / BackgroundTool + dispatch registry
runic-context-engine      ContextEngine trait + Composite + Compactor + Spillover + Reminder
runic-agent-core          Agent loop, hooks, state, sub-agent dispatch
runic-skills              SKILL.md parser, registry, layer, view tool
runic-agents              AGENT.md parser, registry, conversion to SubagentTool
runic-plugins             ~/.runic/plugins/{name}/ discovery, aggregate registries
runic-mcp                 MCP client (stdio + Streamable HTTP transports)
runic                     REPL binary that wires everything together
runic-examples            runnable examples (this is the new one)
```

The dependency DAG is documented in [ARCHITECTURE.md](./ARCHITECTURE.md#crate-dependency-graph).

## Environment knobs

| Variable | Purpose | Default |
|---|---|---|
| `RUNIC_HOME` | Root for skills, agents, plugins, mcp.json, memory | `~/.runic` |
| `RUNIC_PROVIDER` | `anthropic` or `gemini` | `anthropic` |
| `RUNIC_MODEL` | Provider model override | provider default |
| `ANTHROPIC_API_KEY` | Required when provider is Anthropic | — |
| `GEMINI_API_KEY` | Required when provider is Gemini | — |
| `RUNIC_SPILLOVER_THRESHOLD` | Bytes above which a tool result gets spilled | `8192` |
| `RUNIC_COMPACT_THRESHOLD` | Token count above which to compact | `100000` |

## What's not built yet

- Persistence (event log writer/replayer)
- Serve mode (HTTP/socket daemon)
- Blob / file uploads
- Slash commands

See the roadmap section of [ARCHITECTURE.md](./ARCHITECTURE.md#whats-not-built).
