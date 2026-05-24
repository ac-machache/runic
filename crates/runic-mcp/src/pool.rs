//! `SharedMcpPool` — daemon-wide pool for stateless MCP servers.
//!
//! When multiple sessions all want to talk to the same stateless server
//! (e.g. a GitHub API wrapper), spawning one subprocess per session is
//! wasteful. The pool keeps one connection alive and hands out cloneable
//! [`McpHandle`]s to anyone who asks.
//!
//! Two patterns to know:
//!
//! 1. **Connect deduplication via `Notify`.** When two sessions race to
//!    acquire the same server, the second one parks on a `Notify` and
//!    waits for the first to finish — instead of either both spawning
//!    duplicate subprocesses, or one blocking under a `Mutex` for the
//!    entire connect duration.
//!
//! 2. **Reference counting.** Each `acquire` bumps a count; `release`
//!    decrements. When the count reaches zero (no live sessions hold a
//!    handle), the server is eligible for shutdown — though we keep it
//!    around until explicit `shutdown` to avoid spawn churn.
//!
//! Stateful servers (`shared: false` in config) must NOT use the pool;
//! they go through [`crate::manager::McpManager`] which spawns them
//! per-session and owns them outright.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify, RwLock};
use tracing::{debug, warn};

use crate::client::{McpClient, McpHandle};
use crate::config::McpServerConfig;
use crate::error::McpError;
use crate::tool::McpTool;

/// Cooldown after a failed connect attempt — retries within this window
/// short-circuit so we don't hammer a broken server.
const FAILED_CONNECT_RETRY_COOLDOWN: Duration = Duration::from_secs(30);

/// One pooled entry: the live client + a refcount.
struct PoolEntry {
    client: McpClient,
    ref_count: usize,
}

/// State we track for connect attempts in progress, so concurrent callers
/// can wait on the first attempt instead of duplicating it.
enum ConnectAttempt {
    /// Someone is actively connecting. Park on `notify` until they finish.
    InFlight { notify: Arc<Notify> },
    /// A previous attempt failed at this `Instant`. New attempts within
    /// [`FAILED_CONNECT_RETRY_COOLDOWN`] short-circuit with an error.
    FailedAt(Instant),
}

#[derive(Default)]
pub struct SharedMcpPool {
    /// The live entries, keyed by server name. `Mutex` because we mutate
    /// the entry (ref_count) and add/remove keys.
    entries: Mutex<HashMap<String, PoolEntry>>,
    /// Quick read-mostly view of just the handles — cloneable without
    /// holding the entries mutex. Updated whenever entries change.
    handles: RwLock<HashMap<String, McpHandle>>,
    /// Coordination for concurrent connect attempts.
    attempts: Mutex<HashMap<String, ConnectAttempt>>,
}

impl SharedMcpPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire a handle to the named server. If the server is already in
    /// the pool, bumps the refcount and returns the cached handle. If
    /// not, spawns it (deduplicating concurrent acquires).
    ///
    /// Errors:
    ///   - `McpError::Spawn` / `McpError::JsonRpc` / etc. from the underlying
    ///     [`McpClient::connect`]
    ///   - Synthetic error if a recent connect attempt failed (cooldown)
    pub async fn acquire(
        &self,
        server_name: &str,
        config: &McpServerConfig,
    ) -> Result<McpHandle, McpError> {
        // Fast path: server already live.
        if let Some(handle) = self.handles.read().await.get(server_name).cloned() {
            self.bump_refcount(server_name).await;
            return Ok(handle);
        }

        // Connect-dedup path.
        loop {
            let notify = {
                let mut attempts = self.attempts.lock().await;
                match attempts.get(server_name) {
                    Some(ConnectAttempt::InFlight { notify }) => {
                        // Someone else is connecting — wait on their notify.
                        notify.clone()
                    }
                    Some(ConnectAttempt::FailedAt(when)) => {
                        if when.elapsed() < FAILED_CONNECT_RETRY_COOLDOWN {
                            return Err(McpError::protocol(format!(
                                "server '{server_name}' is in failed-connect cooldown ({:?} remaining)",
                                FAILED_CONNECT_RETRY_COOLDOWN.saturating_sub(when.elapsed())
                            )));
                        }
                        // Cooldown elapsed — fall through to start a new attempt.
                        attempts.remove(server_name);
                        let notify = Arc::new(Notify::new());
                        attempts.insert(
                            server_name.to_string(),
                            ConnectAttempt::InFlight {
                                notify: notify.clone(),
                            },
                        );
                        drop(attempts);
                        return self.do_connect(server_name, config, notify).await;
                    }
                    None => {
                        // We're the first to try — register our intent + notify.
                        let notify = Arc::new(Notify::new());
                        attempts.insert(
                            server_name.to_string(),
                            ConnectAttempt::InFlight {
                                notify: notify.clone(),
                            },
                        );
                        drop(attempts);
                        return self.do_connect(server_name, config, notify).await;
                    }
                }
            };
            // Wait for the in-flight attempt to wake us, then re-check.
            notify.notified().await;
            if let Some(handle) = self.handles.read().await.get(server_name).cloned() {
                self.bump_refcount(server_name).await;
                return Ok(handle);
            }
            // If we get here, the in-flight attempt failed — fall through
            // and re-evaluate the attempts state (which now contains
            // FailedAt or has been removed entirely).
        }
    }

    /// Drop one reference to the named server. When the refcount hits 0
    /// the entry stays in the pool — call [`Self::shutdown`] to actually
    /// kill the subprocess.
    pub async fn release(&self, server_name: &str) {
        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get_mut(server_name) {
            entry.ref_count = entry.ref_count.saturating_sub(1);
            debug!(
                server = server_name,
                ref_count = entry.ref_count,
                "MCP pool release"
            );
        }
    }

    /// Build [`McpTool`]s for every tool exposed by the given server.
    /// Returns empty vec if the server isn't in the pool.
    pub async fn tools_for(&self, server_name: &str) -> Vec<Arc<McpTool>> {
        let entries = self.entries.lock().await;
        let Some(entry) = entries.get(server_name) else {
            return Vec::new();
        };
        entry
            .client
            .tools()
            .iter()
            .map(|def| Arc::new(McpTool::new(entry.client.handle().clone(), def.clone())))
            .collect()
    }

    /// Currently-pooled server names, sorted.
    pub async fn server_names(&self) -> Vec<String> {
        let entries = self.entries.lock().await;
        let mut names: Vec<String> = entries.keys().cloned().collect();
        names.sort();
        names
    }

    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.entries.lock().await.is_empty()
    }

    /// Total live references across all entries — useful for debugging
    /// "why is this server still alive?".
    pub async fn total_refs(&self) -> usize {
        self.entries
            .lock()
            .await
            .values()
            .map(|e| e.ref_count)
            .sum()
    }

    /// Forcefully shut down one server. Drops its [`McpClient`] (which
    /// kills the subprocess via `kill_on_drop`).
    pub async fn shutdown(&self, server_name: &str) {
        let removed = {
            let mut entries = self.entries.lock().await;
            entries.remove(server_name)
        };
        if let Some(entry) = removed {
            self.handles.write().await.remove(server_name);
            entry.client.shutdown().await;
        }
    }

    /// Shut every pooled server down. After this the pool is empty.
    pub async fn shutdown_all(&self) {
        let drained: Vec<(String, PoolEntry)> = {
            let mut entries = self.entries.lock().await;
            entries.drain().collect()
        };
        self.handles.write().await.clear();
        for (name, entry) in drained {
            debug!(server = %name, "shutting down pooled MCP server");
            entry.client.shutdown().await;
        }
    }

    // ─── Internals ──────────────────────────────────────────────────────

    async fn bump_refcount(&self, server_name: &str) {
        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get_mut(server_name) {
            entry.ref_count += 1;
        }
    }

    /// Actually perform the connect and update bookkeeping. Always wakes
    /// the notify and removes the InFlight attempt entry on the way out
    /// (success OR failure) so other waiters can proceed.
    async fn do_connect(
        &self,
        server_name: &str,
        config: &McpServerConfig,
        notify: Arc<Notify>,
    ) -> Result<McpHandle, McpError> {
        let result = McpClient::connect(server_name, config).await;
        let mut attempts = self.attempts.lock().await;
        match result {
            Ok(client) => {
                let handle = client.handle().clone();
                {
                    let mut entries = self.entries.lock().await;
                    entries.insert(
                        server_name.to_string(),
                        PoolEntry {
                            client,
                            ref_count: 1,
                        },
                    );
                }
                self.handles
                    .write()
                    .await
                    .insert(server_name.to_string(), handle.clone());
                attempts.remove(server_name);
                drop(attempts);
                notify.notify_waiters();
                Ok(handle)
            }
            Err(err) => {
                warn!(
                    server = server_name,
                    error = %err,
                    "MCP pool connect failed; entering {FAILED_CONNECT_RETRY_COOLDOWN:?} cooldown"
                );
                attempts.insert(
                    server_name.to_string(),
                    ConnectAttempt::FailedAt(Instant::now()),
                );
                drop(attempts);
                notify.notify_waiters();
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_pool_is_empty() {
        let pool = SharedMcpPool::new();
        assert!(pool.is_empty().await);
        assert_eq!(pool.len().await, 0);
        assert_eq!(pool.total_refs().await, 0);
        assert!(pool.server_names().await.is_empty());
    }

    #[tokio::test]
    async fn acquire_missing_binary_returns_error_and_records_cooldown() {
        let pool = SharedMcpPool::new();
        let cfg = McpServerConfig::Stdio(crate::config::StdioServerConfig {
            command: "/definitely/not/here".into(),
            args: vec![],
            env: Default::default(),
            shared: true,
        });
        let first = pool.acquire("ghost", &cfg).await;
        assert!(first.is_err());

        // Immediate retry should short-circuit on cooldown — no second
        // spawn attempt.
        let second = pool.acquire("ghost", &cfg).await;
        match second {
            Err(McpError::Protocol(msg)) => {
                assert!(msg.contains("cooldown"), "got: {msg}");
            }
            other => panic!("expected cooldown error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn release_on_unknown_server_is_a_noop() {
        let pool = SharedMcpPool::new();
        pool.release("nobody").await;
        assert!(pool.is_empty().await);
    }

    #[tokio::test]
    async fn tools_for_unknown_server_returns_empty() {
        let pool = SharedMcpPool::new();
        assert!(pool.tools_for("nobody").await.is_empty());
    }

    #[tokio::test]
    async fn shutdown_unknown_server_is_a_noop() {
        let pool = SharedMcpPool::new();
        pool.shutdown("nobody").await;
        assert!(pool.is_empty().await);
    }
}
