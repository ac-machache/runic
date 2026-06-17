# Persistence

Persistence in runic is **a layer on top of the agent**, not bolted into
it. The agent broadcasts every `SessionEvent` it produces; external code
subscribes and writes them wherever it wants.

This decoupling means:
- The agent has no idea persistence exists
- Multiple consumers can subscribe (persister + audit log + UI mirror)
- Swapping backends (file → Postgres → Redis) is a new `impl`, never a fork

## The two primitives

### 1. Agent event stream

```rust
impl Agent {
    pub fn subscribe_events(&self) -> broadcast::Receiver<SessionEvent>;
}
```

`tokio::sync::broadcast` channel with capacity 1024. Every call to
`state.push_event(...)` fans out to all subscribers. A subscriber that
falls behind sees `RecvError::Lagged(n)` — explicit, never silent.

### 2. The `SessionStore` trait

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn append(&self, tenant: &str, session_id: &str, event: &SessionEvent)
        -> Result<u64, StoreError>;
    async fn read(&self, tenant: &str, session_id: &str)
        -> Result<Vec<StoredEvent>, StoreError>;
    async fn read_after(&self, tenant: &str, session_id: &str, after_seq: u64)
        -> Result<Vec<StoredEvent>, StoreError>;
    async fn list_sessions(&self, tenant: &str) -> Result<Vec<String>, StoreError>;
    async fn list_tenants(&self) -> Result<Vec<String>, StoreError>;
    async fn delete_session(&self, tenant: &str, session_id: &str)
        -> Result<(), StoreError>;
}
```

**Multi-tenant first-class.** Every operation is scoped by `(tenant,
session_id)`. The store guarantees isolation — `list_sessions("alice")`
never returns Bob's data.

**Store assigns seq.** Callers never manage ordering. `append` returns
the assigned sequence number for that event. Read always returns events
in seq order.

## Wiring it up

The whole integration is three lines:

```rust
use runic_sessions::{spawn_persister, FileSessionStore, SessionStore};

let store: Arc<dyn SessionStore> = Arc::new(FileSessionStore::new(storage.clone()));
let _handle = spawn_persister(
    agent.subscribe_events(),
    store,
    "alice".to_string(),                       // tenant
    agent.state().session_id.clone(),          // session id
);
```

That's it. The persister task drains the broadcast and writes to the
store. The agent runs normally; it has no awareness that persistence
is happening.

## Reference impl: `FileSessionStore`

Built on `Arc<dyn StorageBackend>` — so it works against any backend
that implements that trait (`LocalFsBackend`, `MemoryBackend`,
`OverlayBackend`, `NamespacedBackend`, plus any cloud backend a user
implements).

Layout:
```
{root}/{tenant}/{session_id}/events.jsonl
```

Each line is a JSON object:
```json
{ "seq": 1, "event": { "kind": "RunStart", "run_id": "...", "at": "..." } }
{ "seq": 2, "event": { "kind": "Message", "run_id": "...", "msg": {...}, "at": "..." } }
```

Append-only. Atomic per-line at the OS level on most filesystems. Safe
for a single writer per `(tenant, session)` — which is the expected
pattern (one persister per agent run).

## Replay

```rust
use runic_sessions::{replay_into_state, replay_messages};

// Full rebuild — returns an AgentState ready to hand to AgentBuilder
let state = replay_into_state(&*store, "alice", "thread-1", DEFAULT_SYSTEM_PROMPT).await?;

// Just the messages — for showing history in a UI
let msgs = replay_messages(&*store, "alice", "thread-1").await?;
```

`replay_into_state` rebuilds the full event log into a fresh
`AgentState`, ready to resume from. Pass it via `AgentBuilder::session_id`
(plus all the other config) and you've got a resumed session.

`replay_messages` is the lighter shortcut — useful when you want to
DISPLAY a thread's conversation (in a web UI, a CLI inspector) without
rebuilding the agent.

## Listing threads for a user

```rust
let sessions = store.list_sessions("alice").await?;
for sid in &sessions {
    println!("alice's thread: {sid}");
}
```

That's the entire "list user's threads" path. The store's tenant
scoping handles isolation; the listing is just an alphabetized list of
session IDs.

For tenants themselves:
```rust
let tenants = store.list_tenants().await?;
```

Optional — backends that can't enumerate (some auth-context-driven
schemes) return `StoreError::Unsupported`.

## Built-in backends

Two ship in `runic-sessions`:

- **`FileSessionStore`** — appends to
  `sessions/{tenant}/{session_id}/events.jsonl` over any `StorageBackend`.
  Zero infra; the default for local runs.
- **`PostgresSessionStore`** — behind the `postgres` feature. One
  `session_events` table keyed by `(tenant, session_id)` with a `seq`
  column for atomic per-session ordering. Connect with
  `PostgresSessionStore::connect(&database_url).await?` (creates the
  schema if absent).

The reference server picks between them at boot: **Postgres when
`DATABASE_URL` is set, else the file store**.

```rust
let store: Arc<dyn SessionStore> = match std::env::var("DATABASE_URL") {
    Ok(url) => Arc::new(PostgresSessionStore::connect(&url).await?),
    Err(_)  => Arc::new(FileSessionStore::new(fs_backend)),
};
```

## Adding your own backend

Implement the four-method `SessionStore` trait (append / read /
list_sessions / delete) over whatever store you like — Redis, S3, a
managed log. `PostgresSessionStore` (`crates/runic-sessions/src/postgres.rs`)
is the reference for a real database backend.

## Composability: store decorators

`SessionStore` is a trait — you can wrap one in another:

```rust
pub struct AuditingSessionStore<S> {
    inner: Arc<S>,
    audit_sink: Arc<dyn AuditLogger>,
}

#[async_trait]
impl<S: SessionStore> SessionStore for AuditingSessionStore<S> {
    async fn append(&self, tenant: &str, session_id: &str, event: &SessionEvent)
        -> Result<u64, StoreError>
    {
        self.audit_sink.log_append(tenant, session_id);
        self.inner.append(tenant, session_id, event).await
    }
    // ... forward the rest, adding behavior as needed
}
```

Useful decorator ideas (each ~30-50 lines):

| Decorator | What it adds |
|---|---|
| `AuditingSessionStore` | Logs every operation to your audit sink |
| `CachingSessionStore` | LRU cache for `list_sessions` / `read` results |
| `EncryptingSessionStore` | Encrypts event payloads at rest |
| `RoutingSessionStore` | Routes tenant A to Postgres, tenant B to S3 |
| `RateLimitedSessionStore` | Per-tenant write rate limits |
| `MetricsSessionStore` | Prometheus counters / histograms per op |

Compose freely:
```rust
let store: Arc<dyn SessionStore> = Arc::new(FileSessionStore::new(storage));
let store: Arc<dyn SessionStore> = Arc::new(AuditingSessionStore::new(store, audit));
let store: Arc<dyn SessionStore> = Arc::new(CachingSessionStore::new(store, 1024));
```

Same pattern as Tower middleware in the HTTP world, or `ContextEngine`
decorators in this crate.

## Binary / server integration

The reference HTTP server (`cargo run -p runic`) persists every run with
no flag: it picks Postgres when `DATABASE_URL` is set, else the file
store under `~/.runic/sessions`. `runic-serve`'s `ThreadPool` spawns a
`spawn_persister` once per agent — it subscribes to the agent's event
broadcast and appends each `SessionEvent` to the store keyed by
`(tenant, thread)`. A thread that goes cold is rebuilt from its persisted
events on the next request.

`runic-serve` already wraps these `SessionStore` primitives as HTTP:

```
- POST   /threads                          ← create thread
- GET    /threads                          ← list_sessions(tenant)
- GET    /threads/:tid/events              ← store.read(tenant, tid)
- POST   /threads/:tid/runs/stream         ← spawn run + persister (SSE)
- DELETE /threads/:tid                     ← store.delete_session(tenant, tid)
```

The tenant comes from the `X-Runic-Tenant` header.

## What's deferred

| Deferred | When to revisit |
|---|---|
| `MemoryStore` (hierarchical KV for long-term memory across sessions) | After SessionStore is stable; different consistency semantics |
| Checkpoint forking ("branch from this event in the past") | When you actually need time-travel beyond simple replay |
| Schema migration | When `SessionEvent` evolves and you need to read old logs |
| Compression of events.jsonl | When files get huge (>100MB) |
| Vector indexing for semantic event search | When debugging via grep gets too slow |

## Recommended reading

- `crates/runic-sessions/src/store.rs` — the `SessionStore` trait
- `crates/runic-sessions/src/file.rs` — `FileSessionStore` reference impl
- `crates/runic-sessions/src/persister.rs` — the broadcast → store glue
- `crates/runic-sessions/src/replay.rs` — replay helpers
- `crates/runic-sessions/tests/integration.rs` — 15 tests covering
  the full surface
- `crates/runic-agent-core/src/state.rs` — how the broadcast channel is
  installed and how `push_event` fans out
