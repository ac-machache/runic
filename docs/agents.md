# Markdown Sub-agents

A **sub-agent** is a child agent the parent can invoke as a tool. Sub-agents
have:

- Their own fresh `AgentState` (no parent transcript leakage)
- Their own system prompt (focused on the delegated task)
- Their own tool set (typically smaller than the parent's)
- A return value: the child's last assistant text becomes the tool result

You can define sub-agents in Rust via `SubagentTool::new(...)` — but most
of the time you don't have to. Drop an `AGENT.md` file and you're done.

## File format

```
~/.runic/agents/researcher/AGENT.md
```

```yaml
---
name: researcher
description: Investigate a topic and return a concise summary
max-turns: 8
---
You are a focused research sub-agent. Investigate the user's prompt
thoroughly and return a summary in 3-6 sentences.

Do not ask clarifying questions — make reasonable assumptions and
answer. Cite sources when relevant.
```

Required frontmatter: `name`, `description`.
Optional: `max-turns` (default 16; smaller than parent's default of 64
to keep sub-agents focused).

Unknown fields (`model`, `allowed-tools`, etc.) are silently ignored
so Claude Code's `agents/*.md` files paste in without modification.

## How the parent sees it

Once loaded, each `AGENT.md` becomes a regular `Tool` in the parent's
registry. The model calls it like any other tool:

```
researcher({ "prompt": "What's the difference between RSA and ECDSA?" })
```

The tool's input schema is fixed:

```json
{
  "type": "object",
  "properties": {
    "prompt": {
      "type": "string",
      "description": "Instructions for the subagent. Be specific and self-contained — the subagent has a fresh context and cannot see your conversation."
    }
  },
  "required": ["prompt"]
}
```

The child runs to completion (up to `max-turns`), and its last assistant
text comes back as the tool result.

## Programmatic access

```rust
use runic_agents::{AgentRegistry, MdAgent};
use runic_storage_backend::LocalFsBackend;
use std::sync::Arc;

let storage = Arc::new(LocalFsBackend::new("~/.runic"));
let registry = AgentRegistry::load(storage, "agents").await?;

// Convert each MdAgent into a runnable SubagentTool
for md_agent in registry.list() {
    let tool = md_agent.make_subagent_tool(provider.clone());
    builder = builder.tool(Arc::new(tool));
}
```

`make_subagent_tool` takes just an `Arc<dyn Provider>` — the minimal case
(textual prompt, no tools/skills). Sub-agents run on whatever provider
you pass, so give a child a cheaper model than the parent by passing a
different one.

For scoped tools/skills, a persister, or cross-cutting hooks, use
`make_subagent_tool_with_context`, which takes a single `SubagentSetup`:

```rust
use runic_agents::SubagentSetup;
use runic_agent_core::CallLimitHook;

let tool = md_agent.make_subagent_tool_with_context(SubagentSetup {
    provider: cheap_provider.clone(),     // child's model
    parent_pool: pool.clone(),            // child's allowed_tools resolve against this
    parent_skills: skills.clone(),        // scoped to the child's `skills:` list
    storage: backend.clone(),             // filesystem / skill tools bind here
    skills_root: "skills",
    persister: None,                      // Some(fn) to persist the child's events
    hooks: vec![Arc::new(CallLimitHook::default().limit("search", 3))],
});
```

`hooks` are installed on the child agent, so a cross-cutting cap (or any
`Hook`) applies inside sub-agents too — each caps its own run.

## Sub-agent from a plugin

Just like skills, sub-agents can ship as part of a plugin:

```
~/.runic/plugins/code-review/agents/reviewer/AGENT.md
```

The plugin manager loads them and merges into the same flat agent
registry. See [plugins.md](./plugins.md).

## Sync vs async sub-agents

`make_subagent_tool` returns a synchronous `SubagentTool` — the parent
waits for the child to finish before continuing.

For long-running investigations, use `AsyncSubagentTool` instead (this
isn't currently exposed via `AGENT.md` — Rust only):

```rust
use runic_agent_core::AsyncSubagentTool;

let deep_research = AsyncSubagentTool::new(
    "deep_research",
    "Spawn an ASYNCHRONOUS sub-agent for longer investigations. Returns a task_id immediately; check progress with background_status(task_id) and read the result when status is 'done'.",
    move || Agent::builder(provider.clone()).system_prompt("...").build(),
);
```

The parent gets a task id back, can keep working, and polls via
`background_status` or gets notified via `BackgroundTaskReminder`.

## Recursion safety

A sub-agent can itself spawn sub-agents — if its factory closure
registers `SubagentTool` instances. There's NO built-in recursion
depth limit; if you want to prevent unbounded fan-out, simply don't
include any `SubagentTool` in the child's `AgentBuilder`.

The default `MdAgent::make_subagent_tool` does NOT pass sub-agents to
the child. The child has no sub-agent tools, only whatever the
provider call returns. Safe by default.

## When NOT to use a sub-agent

Sub-agents are good for:
- **Exploration** — research, web search, multi-step reasoning that
  shouldn't pollute the parent transcript
- **Specialization** — a focused reviewer/critic/writer that has a
  different system prompt than the main agent
- **Cost optimization** — cheap model for grunt work, expensive model
  for the parent

Sub-agents are NOT for:
- **Simple tool calls** — write a regular `Tool` if you don't need a
  full LLM conversation
- **Anything stateful between calls** — each invocation is a fresh agent

## Recommended reading

- `crates/runic-agents/src/lib.rs` — `MdAgent` + parser + tests
- `crates/runic-agent-core/src/subagent.rs` — `SubagentTool` and
  `AsyncSubagentTool` impl
- `crates/runic-agent-core/src/agent.rs` — the `Agent` machinery that
  the sub-agent factory builds
