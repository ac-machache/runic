# MCP

`runic-mcp` is a Model Context Protocol client. It connects to MCP
servers, discovers the tools they expose, and wraps each one as a
regular `Tool` the agent can call. Two transports supported:

- **stdio** — spawns a binary as a subprocess; talks line-delimited
  JSON over stdin/stdout. The standard for local servers (GitHub
  MCP, filesystem MCP, sentry, etc.).
- **Streamable HTTP** — POSTs JSON-RPC to a single URL; reads JSON or
  Server-Sent Events responses. The 2025-03-26 spec successor to the
  older HTTP+SSE protocol. Use for remote servers.

## Config

`~/.runic/mcp.json`:

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "uvx",
      "args": ["mcp-server-filesystem", "/tmp"]
    },
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": {
        "GITHUB_TOKEN": "ghp_..."
      }
    },
    "remote-api": {
      "url": "https://mcp.example.com/messages",
      "headers": {
        "Authorization": "Bearer your-token-here"
      }
    }
  }
}
```

Two server shapes; the parser distinguishes them by which field is
present:

| Field | Transport |
|---|---|
| `command` (+ optional `args`, `env`) | stdio |
| `url` (+ optional `headers`) | HTTP |

The schema matches Claude Desktop / Cursor's `mcpServers` format
exactly, so existing config files paste in without modification (for
the stdio case; HTTP isn't standardized across other clients yet).

## Tool naming

Every MCP tool gets registered under a prefixed name:

```
mcp__{server_name}__{tool_name}
```

So GitHub's `list_repositories` tool becomes `mcp__github__list_repositories`
in the agent's registry. This prevents collisions with native tools and
with other MCP servers.

## Server lifecycle (stdio)

When the binary starts:

1. Parses `~/.runic/mcp.json` (missing file = silently skip everything)
2. Spawns every configured server in parallel via `tokio::process::Command`
3. Does the MCP `initialize` handshake against each one
4. Lists each server's tools and wraps them as `McpTool`s

Per-server failures are isolated — if `github` won't spawn, you still
get `filesystem` and `remote-api`. A warning gets logged for each
failure.

When the binary exits, each server gets a best-effort `shutdown`
notification and is then `kill`-ed via `tokio::process::Child::kill`
(also `kill_on_drop`-protected).

## Stdio: how requests flow

```
Agent → mcp__filesystem__read_file
      → McpTool::execute
      → McpHandle::call_tool("read_file", { path: "..." })
      → McpHandle::request("tools/call", { name, arguments })
      → StdioTransport::request
        ├─ writes JSON-RPC request line to subprocess stdin
        ├─ awaits response on its oneshot for that request id
        └─ reader task delivers response from subprocess stdout
      → returns parsed result
```

Request/response correlation: a monotonic `u64` per request, tracked
in `HashMap<u64, oneshot::Sender>`. 30-second response timeout.

## HTTP: how requests flow

```
Agent → mcp__remote-api__some_tool
      → ... (same as stdio up to HttpTransport)
      → HttpTransport::request
        ├─ POST { jsonrpc, id, method, params } to the configured URL
        ├─ inspect Content-Type:
        │     application/json     → parse JSON-RPC response directly
        │     text/event-stream    → consume SSE events until one with
        │                            our request id arrives
        └─ track Mcp-Session-Id header across requests
      → returns parsed result
```

No persistent connection between requests — each call is a standalone
POST. This is the "synchronous mode" of Streamable HTTP. The
"long-running" mode (server pushes multiple SSE events per request)
is supported on the response side: we walk the SSE stream and pick
the event with our request id.

## Custom headers

Use HTTP servers behind auth:

```json
{
  "mcpServers": {
    "secured": {
      "url": "https://internal.example.com/mcp",
      "headers": {
        "Authorization": "Bearer abc",
        "X-Tenant": "acme",
        "X-Trace-Id": "runic-mcp"
      }
    }
  }
}
```

Headers are sent on every request to that server. Invalid header
names/values are rejected at config-load time.

## Per-server flags

```json
{
  "mcpServers": {
    "browser": {
      "command": "node",
      "args": ["my-stateful-server.js"],
      "shared": false
    }
  }
}
```

- `shared: true` (default) — server is reusable across sessions via
  `SharedMcpPool` (the HTTP server shares one pool across threads)
- `shared: false` — server is per-session; never pooled. Use for
  stateful servers (browser drivers, IDE handles, etc.) where two
  sessions sharing one process would corrupt state.

## Programmatic access

```rust
use runic_mcp::{McpConfig, McpManager};

let config = McpConfig::try_load_from_path("~/.runic/mcp.json").await?
    .unwrap_or_default();

let manager = McpManager::connect_all(&config).await;

println!("{} server(s) connected, {} tool(s) total",
    manager.len(), manager.total_tool_count());

// Register every MCP-provided tool with the agent
for tool in manager.all_tools() {
    builder = builder.tool(tool);
}
```

`McpManager::connect_all` is best-effort — it never fails the whole
call because one server didn't connect. Successful servers are
returned in the manager; failed ones are logged and skipped.

## What's not supported

| Feature | Notes |
|---|---|
| `resources/*` endpoints | Types are defined (in `protocol.rs`), endpoints not wired |
| `prompts/*` endpoints | Same as above |
| Sampling (server → LLM) | Spec'd but rare in practice; not implemented |
| Server → client notifications | Logged as debug, not surfaced |
| Logging endpoint | Not implemented |
| Progress notifications | Not implemented |
| Roots | Not implemented |
| Old HTTP+SSE transport (2024-11-05) | Use Streamable HTTP instead (most servers support both) |

Each is additive — implement when you hit a server that needs it.

## What's planned

When serve mode lands, `SharedMcpPool` becomes useful — multiple
sessions hitting the same stateless server (e.g. a wrapper around a
public API) share one subprocess via refcounted handles. The pool
already ships; it's just not wired into the binary yet because there's
only one session at a time.

## Testing

The crate ships 43 tests covering protocol serialization (JSON-RPC
round-trips, MCP-specific message shapes), config parsing (stdio +
HTTP + mixed), transport-level error paths (subprocess spawn fail,
network unreachable, etc.), and tool-prefix naming.
