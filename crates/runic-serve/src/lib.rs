//! `runic-serve` — HTTP server that exposes a runic Runner over the wire.
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
//!   - Token validation (consumer-owned via [`auth::IdentityResolver`])
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
pub mod auth;
pub mod completion;
pub mod error;
pub mod hosts;
pub mod openapi;
pub mod routes;
pub mod store;
pub mod stream;
pub mod tenant;
pub mod wire;
pub mod worker;

pub use app::{AppState, ServeConfig, bare_router, router, serve, single_agent};
pub use auth::{Identity, IdentityError, IdentityResolver};
pub use error::ServeError;
pub use hosts::{AgentRegistry, HostedAgents};
pub use sqlx::PgPool;
pub use store::{RunRecord, RunSpec, RunStatus, Runs};
#[cfg(feature = "redis")]
pub use stream::RedisEvents;
pub use stream::{RunEmitter, RunEvents};
pub use tenant::Tenant;
pub use wire::WireEvent;
pub use worker::Worker;
