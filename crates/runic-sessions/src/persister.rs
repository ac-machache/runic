//! [`spawn_persister`] — drain the agent's event broadcast into a
//! [`crate::SessionStore`].
//!
//! Typical use:
//!
//! ```ignore
//! let agent = Agent::builder(provider).build();
//! let store: Arc<dyn SessionStore> = Arc::new(FileSessionStore::new(storage));
//!
//! let handle = spawn_persister(
//!     agent.subscribe_events(),
//!     store.clone(),
//!     "alice".to_string(),                          // tenant
//!     agent.state().session_id.clone(),             // session id
//! );
//!
//! // ... run agent ...
//!
//! handle.shutdown().await;                          // graceful drain
//! ```
//!
//! The persister is a free function (plus a handle) — NOT a method on
//! Agent. The agent doesn't know persistence exists; the persister is
//! just one of many possible subscribers.

use std::sync::Arc;

use runic_agent_core::SessionEvent;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::store::SessionStore;

/// Returned by [`spawn_persister`]. Holds the join handle for the writer
/// task so callers can await graceful completion (or abort it).
pub struct PersisterHandle {
    join: JoinHandle<()>,
    tenant: String,
    session_id: String,
}

impl PersisterHandle {
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Wait for the persister task to finish naturally. Persisters
    /// finish when the agent's broadcast channel is closed (i.e. the
    /// Agent is dropped) — call this after you've dropped the Agent.
    pub async fn join(self) -> Result<(), tokio::task::JoinError> {
        self.join.await
    }

    /// Forcefully cancel the persister task. Any events buffered in the
    /// broadcast channel that haven't been written yet are LOST. Prefer
    /// `join` in normal shutdown.
    pub fn abort(self) {
        self.join.abort();
    }
}

/// Subscribe to the agent's event stream and append every event into
/// the given store under `(tenant, session_id)`. The persister runs
/// until the broadcast channel closes (agent dropped) or it gets
/// aborted via [`PersisterHandle::abort`].
///
/// If the store returns an error on append, it's logged via `tracing`
/// and the persister continues — losing a single event is preferred
/// over crashing the writer task and losing every future event.
///
/// If the receiver lags behind (subscriber slower than the agent), it
/// gets `RecvError::Lagged(n)` — also logged and the persister keeps
/// going. The lost events are gone from this subscription; they can't
/// be re-emitted.
pub fn spawn_persister(
    mut rx: broadcast::Receiver<SessionEvent>,
    store: Arc<dyn SessionStore>,
    tenant: String,
    session_id: String,
) -> PersisterHandle {
    let tenant_clone = tenant.clone();
    let session_clone = session_id.clone();
    let join = tokio::spawn(async move {
        debug!(tenant = %tenant_clone, session = %session_clone, "persister started");
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Err(err) = store.append(&tenant_clone, &session_clone, &event).await {
                        warn!(
                            tenant = %tenant_clone,
                            session = %session_clone,
                            error = %err,
                            "persister append failed; event lost"
                        );
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!(
                        tenant = %tenant_clone,
                        session = %session_clone,
                        "persister: agent closed event stream, exiting"
                    );
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(
                        tenant = %tenant_clone,
                        session = %session_clone,
                        skipped = n,
                        "persister lagged; {n} events dropped (subscriber slower than agent)"
                    );
                }
            }
        }
    });

    PersisterHandle {
        join,
        tenant,
        session_id,
    }
}
