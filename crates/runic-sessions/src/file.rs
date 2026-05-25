//! [`FileSessionStore`] — the reference [`SessionStore`] backed by any
//! [`runic_storage_backend::StorageBackend`].
//!
//! On-disk layout:
//!
//! ```text
//! {root}/{tenant}/{session_id}/events.jsonl
//! ```
//!
//! Each line of `events.jsonl` is a JSON object:
//!
//! ```json
//! { "seq": 7, "event": { /* SessionEvent */ } }
//! ```
//!
//! Append-only, line-delimited. Atomic per-line at the OS level on most
//! filesystems; safe for a single writer per `(tenant, session_id)` —
//! which is the expected pattern (one persister per agent run).
//!
//! Seq numbers are assigned per-session in monotonic order. The first
//! event for a session gets `seq = 1`. Internally we cache the
//! next-seq per `(tenant, session_id)` so we don't re-scan the file
//! on every append.

use async_trait::async_trait;
use runic_agent_core::SessionEvent;
use runic_storage_backend::StorageBackend;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::error::StoreError;
use crate::store::{SessionStore, StoredEvent};

const DEFAULT_ROOT: &str = "sessions";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LogLine {
    seq: u64,
    event: SessionEvent,
}

pub struct FileSessionStore {
    storage: Arc<dyn StorageBackend>,
    root: String,
    /// Per-session next-seq cache. Lazy: populated on first append for
    /// each (tenant, session) by scanning the existing file (if any).
    next_seq: Mutex<HashMap<(String, String), u64>>,
}

impl FileSessionStore {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self::with_root(storage, DEFAULT_ROOT)
    }

    pub fn with_root(storage: Arc<dyn StorageBackend>, root: impl Into<String>) -> Self {
        Self {
            storage,
            root: root.into(),
            next_seq: Mutex::new(HashMap::new()),
        }
    }

    fn session_log_path(&self, tenant: &str, session_id: &str) -> String {
        format!("{}/{tenant}/{session_id}/events.jsonl", self.root)
    }

    fn tenant_root(&self, tenant: &str) -> String {
        format!("{}/{tenant}", self.root)
    }

    async fn read_log(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<LogLine>, StoreError> {
        let path = self.session_log_path(tenant, session_id);
        let raw = match self.storage.read_to_string(&path).await {
            Ok(s) => s,
            Err(runic_storage_backend::StorageError::NotFound { .. }) => return Ok(Vec::new()),
            Err(other) => return Err(other.into()),
        };
        let mut out = Vec::new();
        for (lineno, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let parsed: LogLine = serde_json::from_str(line).map_err(|err| {
                StoreError::Storage(format!(
                    "corrupt event log at {path}, line {}: {err}",
                    lineno + 1
                ))
            })?;
            out.push(parsed);
        }
        Ok(out)
    }
}

#[async_trait]
impl SessionStore for FileSessionStore {
    async fn append(
        &self,
        tenant: &str,
        session_id: &str,
        event: &SessionEvent,
    ) -> Result<u64, StoreError> {
        let mut cache = self.next_seq.lock().await;
        let key = (tenant.to_string(), session_id.to_string());

        // Lazy init: if we've never appended to this (tenant, session)
        // before, scan the existing log to learn the current max seq.
        let next = if let Some(n) = cache.get(&key) {
            *n
        } else {
            let existing = self.read_log(tenant, session_id).await?;
            let max_seen = existing.iter().map(|l| l.seq).max().unwrap_or(0);
            let n = max_seen + 1;
            cache.insert(key.clone(), n);
            n
        };

        let line = LogLine {
            seq: next,
            event: event.clone(),
        };
        let mut serialized = serde_json::to_string(&line)?;
        serialized.push('\n');

        let path = self.session_log_path(tenant, session_id);
        self.storage.append(&path, serialized.as_bytes()).await?;

        cache.insert(key, next + 1);
        Ok(next)
    }

    async fn read(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let lines = self.read_log(tenant, session_id).await?;
        let mut out: Vec<StoredEvent> = lines
            .into_iter()
            .map(|l| StoredEvent {
                seq: l.seq,
                event: l.event,
            })
            .collect();
        out.sort_by_key(|s| s.seq);
        Ok(out)
    }

    async fn read_after(
        &self,
        tenant: &str,
        session_id: &str,
        after_seq: u64,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let all = self.read(tenant, session_id).await?;
        Ok(all.into_iter().filter(|s| s.seq > after_seq).collect())
    }

    async fn list_sessions(&self, tenant: &str) -> Result<Vec<String>, StoreError> {
        let prefix = self.tenant_root(tenant);
        let entries = self.storage.list(&prefix).await?;

        // Same dual-mode logic as the other registry loaders:
        //   - hierarchical backend → Directory entries, each is a session
        //   - flat KV backend → File entries with paths like
        //     "{root}/{tenant}/{session_id}/events.jsonl"
        use runic_storage_backend::EntryKind;
        let mut names = std::collections::BTreeSet::new();
        let trimmed = prefix.trim_end_matches('/');
        for entry in &entries {
            let after = entry
                .key
                .strip_prefix(trimmed)
                .map(|s| s.trim_start_matches('/'))
                .unwrap_or(&entry.key);
            let head = match after.split_once('/') {
                Some((h, _)) => h,
                None => after,
            };
            if head.is_empty() {
                continue;
            }
            match entry.kind {
                EntryKind::Directory => {
                    names.insert(head.to_string());
                }
                EntryKind::File if after.ends_with("/events.jsonl") => {
                    names.insert(head.to_string());
                }
                _ => {}
            }
        }
        Ok(names.into_iter().collect())
    }

    async fn list_tenants(&self) -> Result<Vec<String>, StoreError> {
        let entries = self.storage.list(&self.root).await?;
        use runic_storage_backend::EntryKind;
        let mut names = std::collections::BTreeSet::new();
        let trimmed = self.root.trim_end_matches('/');
        for entry in &entries {
            let after = entry
                .key
                .strip_prefix(trimmed)
                .map(|s| s.trim_start_matches('/'))
                .unwrap_or(&entry.key);
            let head = match after.split_once('/') {
                Some((h, _)) => h,
                None => after,
            };
            if head.is_empty() {
                continue;
            }
            match entry.kind {
                EntryKind::Directory => {
                    names.insert(head.to_string());
                }
                EntryKind::File if after.ends_with("/events.jsonl") => {
                    // Walk further: the head is the tenant, the rest is session_id/...
                    names.insert(head.to_string());
                }
                _ => {}
            }
        }
        Ok(names.into_iter().collect())
    }

    async fn delete_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<(), StoreError> {
        let path = self.session_log_path(tenant, session_id);
        match self.storage.delete(&path).await {
            Ok(()) => {}
            Err(runic_storage_backend::StorageError::NotFound { .. }) => {
                // Idempotent — already gone.
            }
            Err(err) => return Err(err.into()),
        }
        let mut cache = self.next_seq.lock().await;
        cache.remove(&(tenant.to_string(), session_id.to_string()));
        Ok(())
    }
}

impl std::fmt::Debug for FileSessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSessionStore")
            .field("root", &self.root)
            .finish()
    }
}
