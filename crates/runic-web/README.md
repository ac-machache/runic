# runic-web

A Leptos CSR (WASM) dev console for `runic serve` — a 3-pane developer UI
instead of curl: thread list, streaming chat with tool-call cards, and a
live state/event inspector.

```
┌───────────┬──────────────────────────┬─────────────────┐
│  threads  │  chat (streaming + tools)│  state / events │
└───────────┴──────────────────────────┴─────────────────┘
```

- **Threads** — create / list / select (per `X-Runic-Tenant`).
- **Chat** — streaming assistant text, collapsible thinking, tool-call
  cards with status + duration + grounding-source chips (from tool-result
  `metadata`).
- **State** — token usage and the live `SessionEvent` log (the same data
  persisted to `events.jsonl`).

## Run it

1. Start the server (see the root README):
   ```sh
   RUNIC_HOME=$PWD/runic-data RUNIC_SERVE_ADDR=127.0.0.1:8920 runic serve
   ```
2. Serve the UI:
   ```sh
   cd crates/runic-web && trunk serve --port 8080
   ```
3. Open <http://127.0.0.1:8080>. The server URL + tenant are editable in
   the top-left (default `http://127.0.0.1:8920`, tenant `default`).

The server's permissive CORS lets the browser app (`:8080`) drive the
server (`:8920`). Events are parsed leniently as JSON, so the UI stays
decoupled from the server's internal `WireEvent` type.

## Requires

`wasm32-unknown-unknown` target and `trunk` (`cargo install trunk`).
