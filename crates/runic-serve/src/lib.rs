//! `runic-serve` — HTTP server that exposes a runic Agent over the wire.
//!
//! Goal: take everything the REPL binary already wires (provider, skills,
//! sub-agents, shell tools, memory, MCP, persistence, blobs) and let
//! a remote client drive it via HTTP + SSE — without the binary having
//! to know what HTTP is.
//!
//! # The crate boundary
//!
//! `runic-serve` knows about:
//!   - Threads (== sessions in our existing vocabulary)
//!   - Runs (one agent invocation on a thread)
//!   - Server-sent events
//!   - The `SessionStore` for durability + replay
//!   - The [`AgentFactory`] trait for spawning agents on demand
//!
//! It does NOT know about:
//!   - Which provider / tools / hooks / skills are wired (the binary
//!     decides via its [`AgentFactory`] impl)
//!   - Auth (just reads a tenant out of the `X-Runic-Tenant` header)
//!   - LangGraph compatibility (this is the runic-native wire format —
//!     a thin direct serialization of our internal events)
//!
//! # Wire format
//!
//! Server-Sent Events. Every event is `{type, ...}` JSON in the `data`
//! field; the SSE `event` field carries the same `type`. Event types are
//! defined in [`wire`].
//!
//! # Resume
//!
//! `GET /threads/:id/runs/:run_id/stream` accepts a `Last-Event-ID`
//! header. The server replays every persisted event whose `seq` is
//! greater than that id, then (if the run is still in flight) attaches
//! to the live broadcast. The `id` field on each SSE event is the
//! store-assigned seq number from [`SessionStore`].

pub mod app;
pub mod error;
pub mod factory;
pub mod pool;
pub mod routes;
pub mod tenant;
pub mod wire;

pub use app::{router, AppState, ServeConfig};
pub use error::ServeError;
pub use factory::{AgentFactory, BoxedAgentFactory};
pub use pool::ThreadPool;
pub use tenant::Tenant;
pub use wire::WireEvent;
