//! `SpilloverEngine` — write huge tool outputs to disk; replace them in
//! context with a summary + path.
//!
//! Wraps any inner [`ContextEngine`] (decorator pattern). On every
//! `maybe_compact` pass, it walks the messages and rewrites any
//! `ContentBlock::ToolResult` whose body is over the configured byte
//! threshold:
//!
//! ```text
//!   <huge tool output, 142 KiB>
//!       ↓
//!   [spilled to spillover/run_id/call_id.txt (142 KiB)]
//!   Preview (first 800 chars):
//!   ...
//! ```
//!
//! The full content lives at the spillover path; the model only sees the
//! preview + path. A tool that knows about the storage backend (filesystem
//! MCP server, a custom read-file tool, etc.) can fetch the rest if needed.
//!
//! State: once a `tool_use_id` is spilled, the replacement is cached so
//! subsequent turns produce the SAME replacement instead of re-spilling.

use async_trait::async_trait;
use runic_message_types::{ContentBlock, Message};
use runic_storage_backend::StorageBackend;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::{AmbientNote, ContextEngine, TurnContext};

/// Default size at which a tool result is considered "huge" and spilled.
/// 8 KiB is small enough that any non-trivial file dump or command output
/// triggers a spill, but big enough that short tool results pass through
/// untouched.
pub const DEFAULT_THRESHOLD_BYTES: usize = 8 * 1024;

/// How many leading characters of the spilled content to keep as a preview
/// in the in-context replacement.
pub const DEFAULT_PREVIEW_CHARS: usize = 800;

#[derive(Debug, Clone)]
struct SpilledInfo {
    /// The replacement text we substituted into the in-context content.
    /// Reused across turns so the rendered prompt is byte-stable.
    replacement: String,
}

pub struct SpilloverEngine {
    inner: Arc<dyn ContextEngine>,
    storage: Arc<dyn StorageBackend>,
    /// Root prefix under the storage backend where spilled files land.
    /// Final path is `{root}/{run_id}/{tool_use_id}.txt`.
    root: String,
    threshold_bytes: usize,
    preview_chars: usize,
    /// tool_use_id → spilled-replacement-text. Built lazily; entries are
    /// retained for the life of the engine so the same tool_use_id always
    /// renders the same in-context placeholder.
    spilled: Mutex<HashMap<String, SpilledInfo>>,
}

impl SpilloverEngine {
    pub fn new(inner: Arc<dyn ContextEngine>, storage: Arc<dyn StorageBackend>) -> Self {
        Self::with_settings(inner, storage, "spillover", DEFAULT_THRESHOLD_BYTES, DEFAULT_PREVIEW_CHARS)
    }

    pub fn with_settings(
        inner: Arc<dyn ContextEngine>,
        storage: Arc<dyn StorageBackend>,
        root: impl Into<String>,
        threshold_bytes: usize,
        preview_chars: usize,
    ) -> Self {
        Self {
            inner,
            storage,
            root: root.into(),
            threshold_bytes,
            preview_chars,
            spilled: Mutex::new(HashMap::new()),
        }
    }

    /// Number of tool results currently spilled (live cache size).
    pub async fn spilled_count(&self) -> usize {
        self.spilled.lock().await.len()
    }

    /// Maybe write `content` to disk and return the replacement text, OR
    /// return `None` if the content is under threshold or already cached.
    async fn maybe_spill_one(
        &self,
        run_id: &str,
        tool_use_id: &str,
        content: &str,
    ) -> Option<String> {
        // Cache hit → reuse previous replacement so the prompt is stable.
        if let Some(info) = self.spilled.lock().await.get(tool_use_id) {
            return Some(info.replacement.clone());
        }

        if content.len() < self.threshold_bytes {
            return None;
        }

        let path = format!("{}/{run_id}/{tool_use_id}.txt", self.root);
        if let Err(err) = self.storage.write(&path, content.as_bytes()).await {
            warn!(
                tool_use_id,
                error = %err,
                "spillover: failed to write — leaving content in-context"
            );
            return None;
        }

        let preview = take_preview(content, self.preview_chars);
        let replacement = format!(
            "[spilled to {path} ({}B)]\n\
             Preview (first {} chars):\n\
             {preview}",
            content.len(),
            self.preview_chars
        );

        let mut guard = self.spilled.lock().await;
        guard.insert(
            tool_use_id.to_string(),
            SpilledInfo {
                replacement: replacement.clone(),
            },
        );
        debug!(tool_use_id, path, bytes = content.len(), "spilled tool result");
        Some(replacement)
    }
}

#[async_trait]
impl ContextEngine for SpilloverEngine {
    async fn assemble_system_prompt(&self, ctx: &TurnContext<'_>) -> String {
        self.inner.assemble_system_prompt(ctx).await
    }

    async fn ambient_notes(&self, ctx: &TurnContext<'_>) -> Vec<AmbientNote> {
        self.inner.ambient_notes(ctx).await
    }

    async fn process_user_input(&self, ctx: &TurnContext<'_>, msg: Message) -> Message {
        self.inner.process_user_input(ctx, msg).await
    }

    async fn maybe_compact(&self, ctx: &TurnContext<'_>, messages: &mut Vec<Message>) {
        // Inner first — anything it does (e.g. summarization) is preserved.
        self.inner.maybe_compact(ctx, messages).await;

        for msg in messages.iter_mut() {
            for block in msg.content.iter_mut() {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = block
                    && let Some(replacement) =
                        self.maybe_spill_one(ctx.run_id, tool_use_id, content).await
                {
                    *content = replacement;
                }
            }
        }
    }
}

impl std::fmt::Debug for SpilloverEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpilloverEngine")
            .field("root", &self.root)
            .field("threshold_bytes", &self.threshold_bytes)
            .field("preview_chars", &self.preview_chars)
            .finish()
    }
}

fn take_preview(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoopEngine;
    use runic_message_types::{ContentBlock, Message, Role};
    use runic_storage_backend::MemoryBackend;

    fn ctx<'a>() -> TurnContext<'a> {
        TurnContext {
            base_system_prompt: "",
            messages: &[],
            run_id: "test-run",
            turn: 0,
        }
    }

    fn tool_result_msg(call_id: &str, content: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: call_id.into(),
                content: content.into(),
                is_error: None,
            }],
            timestamp: None,
            tool_duration_ms: None,
        }
    }

    #[tokio::test]
    async fn small_results_are_not_spilled() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let engine = SpilloverEngine::new(Arc::new(NoopEngine), storage.clone());

        let small = "x".repeat(100);
        let mut messages = vec![tool_result_msg("c1", &small)];
        engine.maybe_compact(&ctx(), &mut messages).await;

        match &messages[0].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, &small, "small content should pass through unchanged");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        assert_eq!(engine.spilled_count().await, 0);
    }

    #[tokio::test]
    async fn large_results_are_spilled_to_storage() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let engine = SpilloverEngine::with_settings(
            Arc::new(NoopEngine),
            storage.clone(),
            "spillover",
            100,
            20,
        );

        let big = "a".repeat(500);
        let mut messages = vec![tool_result_msg("c1", &big)];
        engine.maybe_compact(&ctx(), &mut messages).await;

        match &messages[0].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(content.contains("spilled to spillover/test-run/c1.txt"));
                assert!(content.contains("Preview"));
                assert!(content.contains("500B"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }

        // File should exist in storage with the full content.
        let on_disk = storage
            .read_to_string("spillover/test-run/c1.txt")
            .await
            .unwrap();
        assert_eq!(on_disk, big);

        assert_eq!(engine.spilled_count().await, 1);
    }

    #[tokio::test]
    async fn cached_spill_produces_identical_replacement_across_turns() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let engine = SpilloverEngine::with_settings(
            Arc::new(NoopEngine),
            storage.clone(),
            "spillover",
            50,
            20,
        );

        let big = "x".repeat(300);

        // Turn 1
        let mut m1 = vec![tool_result_msg("call-A", &big)];
        engine.maybe_compact(&ctx(), &mut m1).await;
        let replacement_t1 = match &m1[0].content[0] {
            ContentBlock::ToolResult { content, .. } => content.clone(),
            _ => unreachable!(),
        };

        // Turn 2 — same call id presented again (as if pulled fresh from state)
        let mut m2 = vec![tool_result_msg("call-A", &big)];
        engine.maybe_compact(&ctx(), &mut m2).await;
        let replacement_t2 = match &m2[0].content[0] {
            ContentBlock::ToolResult { content, .. } => content.clone(),
            _ => unreachable!(),
        };

        assert_eq!(
            replacement_t1, replacement_t2,
            "the same call_id must produce a byte-stable replacement across turns"
        );
        assert_eq!(engine.spilled_count().await, 1);
    }

    #[tokio::test]
    async fn distinct_call_ids_get_distinct_spill_paths() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let engine = SpilloverEngine::with_settings(
            Arc::new(NoopEngine),
            storage.clone(),
            "spillover",
            50,
            20,
        );

        let big_a = "a".repeat(200);
        let big_b = "b".repeat(200);
        let mut messages = vec![
            tool_result_msg("call-A", &big_a),
            tool_result_msg("call-B", &big_b),
        ];
        engine.maybe_compact(&ctx(), &mut messages).await;

        assert_eq!(engine.spilled_count().await, 2);
        assert_eq!(
            storage.read_to_string("spillover/test-run/call-A.txt").await.unwrap(),
            big_a
        );
        assert_eq!(
            storage.read_to_string("spillover/test-run/call-B.txt").await.unwrap(),
            big_b
        );
    }

    #[tokio::test]
    async fn delegates_other_methods_to_inner() {
        // Inner that flags its presence on every method.
        #[derive(Debug)]
        struct Marker;
        #[async_trait]
        impl ContextEngine for Marker {
            async fn assemble_system_prompt(&self, _: &TurnContext<'_>) -> String {
                "FROM_INNER".into()
            }
            async fn ambient_notes(&self, _: &TurnContext<'_>) -> Vec<AmbientNote> {
                vec![AmbientNote {
                    source: "inner".into(),
                    content: "tick".into(),
                    dedup_key: None,
                }]
            }
            async fn process_user_input(&self, _: &TurnContext<'_>, mut m: Message) -> Message {
                m.content
                    .insert(0, ContentBlock::Text { text: "TOUCHED_BY_INNER ".into(), cache_control: None });
                m
            }
        }

        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let engine = SpilloverEngine::new(Arc::new(Marker), storage);

        assert_eq!(engine.assemble_system_prompt(&ctx()).await, "FROM_INNER");
        assert_eq!(engine.ambient_notes(&ctx()).await.len(), 1);
        let touched = engine.process_user_input(&ctx(), Message::user("hi")).await;
        match &touched.content[0] {
            ContentBlock::Text { text, .. } => assert!(text.starts_with("TOUCHED_BY_INNER")),
            other => panic!("expected Text block, got {other:?}"),
        }
    }

    #[test]
    fn take_preview_truncates_at_char_boundary() {
        let s = "abcdef";
        assert_eq!(take_preview(s, 3), "abc…");
        assert_eq!(take_preview(s, 10), "abcdef");
        assert_eq!(take_preview("", 5), "");
    }
}
