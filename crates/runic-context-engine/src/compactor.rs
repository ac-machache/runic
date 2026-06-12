//! `CompactorEngine` — summarize old messages when context gets big.
//!
//! Wraps any inner [`ContextEngine`] (decorator pattern). On every
//! `maybe_compact` pass it estimates the message-pile size (chars/4 as the
//! standard cheap token proxy). When over the threshold, it:
//!
//!   1. Sets aside the `keep_recent` most-recent messages verbatim
//!   2. Renders the older messages into text
//!   3. Asks a `Provider` to summarize them
//!   4. Replaces the older messages with a single synthetic message
//!      containing the summary
//!
//! The original messages on disk (in `AgentState.events`) are untouched —
//! `maybe_compact` only mutates the in-memory copy for the current turn,
//! which is rebuilt from events every turn. To avoid paying a summarizer
//! call on *every* turn past the threshold, the engine caches the summary
//! together with a chain-hash of the message prefix it covers. On later
//! turns the same prefix is recognized and swapped for the cached synthetic
//! message with no provider call; when history outgrows the threshold
//! again, the old summary is folded into the next one and the cache
//! extends. A welcome side effect: the replaced prefix is byte-identical
//! across turns, which keeps provider prompt caches warm.
//!
//! The cache lives in memory, so the first over-threshold turn after a
//! process restart pays one summarizer call to rebuild it.

use async_trait::async_trait;
use runic_message_types::{ContentBlock, Message, Role};
use runic_provider_core::Provider;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use crate::{AmbientNote, ContextEngine, TurnContext};

/// Default token threshold (~chars/4) at which we trigger compaction.
/// 100K is a conservative starting point — well below Anthropic's 200K
/// context window, generous enough that short sessions never hit it.
pub const DEFAULT_TOKEN_THRESHOLD: usize = 100_000;

/// Default number of most-recent messages to keep verbatim across
/// compaction. We always want the latest user turn + the latest assistant
/// turn + a little extra so the model has live conversational context.
pub const DEFAULT_KEEP_RECENT: usize = 8;

const DEFAULT_SUMMARIZER_SYSTEM_PROMPT: &str = "\
You are a conversation summarizer. You will be shown a transcript of an \
assistant-model conversation. Produce a concise summary (8-15 sentences) \
that captures:\n\
  - The user's stated goals or questions\n\
  - Key facts the assistant established or discovered\n\
  - Decisions made, tools called, and their important results\n\
  - Any open threads or pending action items\n\
Write in plain prose, third-person past tense. Do not editorialize. The \
summary will replace the original messages in the live conversation, so \
prioritize information the assistant will need to keep working effectively.";

/// Seed for the prefix chain-hash. Arbitrary but fixed — changing it only
/// invalidates in-memory caches, which rebuild on the next compaction.
const CHAIN_SEED: u64 = 0x52_55_4e_49_43; // "RUNIC"

/// A previously computed summary and the message prefix it stands in for.
struct SummaryCache {
    /// How many original (pre-compaction) messages the summary covers.
    covered: usize,
    /// Chain-hash over those messages (role + content only, so per-turn
    /// timestamp stamping can't invalidate it).
    prefix_hash: u64,
    /// Cumulative original messages folded into the summary, for the marker.
    total_compacted: usize,
    /// The synthetic message that replaces the covered prefix.
    synthetic: Message,
}

pub struct CompactorEngine {
    inner: Arc<dyn ContextEngine>,
    summarizer: Arc<dyn Provider>,
    token_threshold: usize,
    keep_recent: usize,
    summarizer_system_prompt: String,
    cache: Mutex<Option<SummaryCache>>,
}

impl CompactorEngine {
    pub fn new(inner: Arc<dyn ContextEngine>, summarizer: Arc<dyn Provider>) -> Self {
        Self {
            inner,
            summarizer,
            token_threshold: DEFAULT_TOKEN_THRESHOLD,
            keep_recent: DEFAULT_KEEP_RECENT,
            summarizer_system_prompt: DEFAULT_SUMMARIZER_SYSTEM_PROMPT.to_string(),
            cache: Mutex::new(None),
        }
    }

    pub fn with_token_threshold(mut self, threshold: usize) -> Self {
        self.token_threshold = threshold;
        self
    }

    pub fn with_keep_recent(mut self, n: usize) -> Self {
        self.keep_recent = n;
        self
    }

    pub fn with_summarizer_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.summarizer_system_prompt = prompt.into();
        self
    }

    fn estimated_tokens(&self, messages: &[Message]) -> usize {
        // chars / 4 — same heuristic Anthropic/OpenAI samples use for
        // back-of-envelope estimation. Good to ~10% on English prose.
        let total_chars: usize = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .map(content_block_char_count)
            .sum();
        total_chars / 4
    }

    async fn summarize(&self, older: &[Message]) -> Option<String> {
        let rendered = render_messages_as_transcript(older);
        let prompt = format!(
            "Below is a transcript of a conversation between a user and an \
             assistant. Summarize it per your instructions.\n\n=== TRANSCRIPT ===\n\
             {rendered}\n=== END TRANSCRIPT ==="
        );
        match self
            .summarizer
            .complete_simple(&prompt, &self.summarizer_system_prompt)
            .await
        {
            Ok(text) => Some(text),
            Err(err) => {
                warn!(error = %err, "compactor: summarization failed; leaving messages untouched");
                None
            }
        }
    }
}

#[async_trait]
impl ContextEngine for CompactorEngine {
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
        self.inner.maybe_compact(ctx, messages).await;

        // 1. Reuse a previous summary if its covered prefix is intact.
        //    Applied unconditionally (not just over threshold) so the
        //    context the provider sees stays byte-stable turn over turn.
        //    `applied` records (covered, prefix_hash, total_compacted) so
        //    the extension step below can build on it.
        let mut applied: Option<(usize, u64, usize)> = None;
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(entry) = cache.as_ref() {
                if messages.len() >= entry.covered
                    && chain_hash(CHAIN_SEED, &messages[..entry.covered]) == entry.prefix_hash
                {
                    messages.drain(..entry.covered);
                    messages.insert(0, entry.synthetic.clone());
                    applied = Some((entry.covered, entry.prefix_hash, entry.total_compacted));
                } else {
                    // The prefix no longer matches what we summarized
                    // (history was rewritten upstream) — the summary is
                    // stale, drop it rather than misrepresent history.
                    debug!("compactor: cached summary no longer matches prefix; discarding");
                    *cache = None;
                }
            }
        }

        let estimated = self.estimated_tokens(messages);
        if estimated < self.token_threshold {
            return;
        }
        if messages.len() <= self.keep_recent {
            return;
        }

        // 2. Still too big — summarize everything but the recent tail. If a
        //    cached summary was applied above it sits at index 0, gets
        //    drained into `older`, and is folded into the new summary.
        let cutoff = messages.len() - self.keep_recent;
        let older: Vec<Message> = messages.drain(..cutoff).collect();
        debug!(
            estimated_tokens = estimated,
            threshold = self.token_threshold,
            compacting_count = older.len(),
            keeping = self.keep_recent,
            reusing_cached_summary = applied.is_some(),
            "compactor: triggering summarization"
        );

        let summary = match self.summarize(&older).await {
            Some(s) => s,
            None => {
                // Summarization failed — restore the messages so we don't
                // silently lose history. (If a cached summary was applied,
                // `older[0]` is that summary — equivalent context, and the
                // cache entry is still valid for next turn.)
                let mut restored = older;
                restored.append(messages);
                *messages = restored;
                return;
            }
        };

        // Cache bookkeeping: coverage and hash are tracked in terms of the
        // ORIGINAL message list (what next turn's rebuild will hand us).
        // When extending, `older[0]` is the previous synthetic summary and
        // everything after it is original messages.
        let (prev_covered, prev_hash, prev_total) =
            applied.unwrap_or((0, CHAIN_SEED, 0));
        let newly_covered = if applied.is_some() { &older[1..] } else { &older[..] };
        let covered = prev_covered + newly_covered.len();
        let prefix_hash = chain_hash(prev_hash, newly_covered);
        let total_compacted = prev_total + newly_covered.len();

        // Prepend a synthetic User message that carries the summary. Using
        // User role (not Assistant) so the summary reads as "background
        // context" to the model rather than "something I said before."
        let synthetic = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format!(
                    "[summary of earlier conversation, {total_compacted} messages compacted]\n{summary}"
                ),
                cache_control: None,
            }],
            timestamp: None,
            tool_duration_ms: None,
        };

        messages.insert(0, synthetic.clone());
        *self.cache.lock().unwrap() = Some(SummaryCache {
            covered,
            prefix_hash,
            total_compacted,
            synthetic,
        });
    }
}

impl std::fmt::Debug for CompactorEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactorEngine")
            .field("token_threshold", &self.token_threshold)
            .field("keep_recent", &self.keep_recent)
            .field("summarizer_model", &self.summarizer.model())
            .finish()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Fingerprint one message from its role + content only. Timestamps and
/// durations are deliberately excluded: the agent loop may stamp those per
/// turn, and they carry no information about what was said.
fn message_fingerprint(msg: &Message) -> u64 {
    let mut h = DefaultHasher::new();
    match msg.role {
        Role::User => 0u8,
        Role::Assistant => 1u8,
    }
    .hash(&mut h);
    for block in &msg.content {
        serde_json::to_string(block).unwrap_or_default().hash(&mut h);
    }
    h.finish()
}

/// Chain-hash a slice of messages onto a previous hash. Because the fold is
/// sequential, `chain_hash(chain_hash(seed, a), b) == chain_hash(seed, a ++ b)`
/// — which is what lets the cache extend its coverage without re-reading
/// messages it already summarized.
fn chain_hash(seed: u64, messages: &[Message]) -> u64 {
    messages.iter().fold(seed, |acc, msg| {
        let mut h = DefaultHasher::new();
        acc.hash(&mut h);
        message_fingerprint(msg).hash(&mut h);
        h.finish()
    })
}

fn content_block_char_count(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text, .. } => text.len(),
        ContentBlock::Reasoning { text } => text.len(),
        ContentBlock::ToolUse { name, input, .. } => {
            // input is a JSON value — count its serialized form as a proxy
            name.len() + serde_json::to_string(input).map(|s| s.len()).unwrap_or(0)
        }
        ContentBlock::ToolResult { content, .. } => content.len(),
        ContentBlock::Image { .. } => 1024, // rough — images are expensive but we don't know exact tokens
        ContentBlock::Blob(b) => {
            // Token cost depends on whether the provider will end up
            // inlining the bytes. Use the declared size as a proxy —
            // overestimate is safer than underestimate for compaction.
            // Bytes / 4 to roughly translate to tokens (same ratio we use
            // for text).
            (b.size as usize) / 4
        }
    }
}

fn render_messages_as_transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
        };
        for block in &msg.content {
            match block {
                ContentBlock::Text { text, .. } => {
                    out.push_str(&format!("[{role}]\n{text}\n\n"));
                }
                ContentBlock::Reasoning { text } => {
                    out.push_str(&format!("[{role} thinking]\n{text}\n\n"));
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    out.push_str(&format!(
                        "[{role} tool call: {name}]\n{}\n\n",
                        serde_json::to_string(input).unwrap_or_default()
                    ));
                }
                ContentBlock::ToolResult { content, .. } => {
                    out.push_str(&format!("[{role} tool result]\n{content}\n\n"));
                }
                ContentBlock::Image { .. } => {
                    out.push_str(&format!("[{role} image attachment]\n\n"));
                }
                ContentBlock::Blob(b) => {
                    let name = b.name.as_deref().unwrap_or("(no name)");
                    out.push_str(&format!(
                        "[{role} blob attachment: {name} ({}, {}B)]\n\n",
                        b.mime, b.size
                    ));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoopEngine;
    use futures::stream;
    use runic_message_types::StreamEvent;
    use runic_provider_core::{EventStream, ProviderError};
    use std::sync::Mutex;

    fn ctx<'a>() -> TurnContext<'a> {
        TurnContext {
            base_system_prompt: "",
            messages: &[],
            run_id: "run-1",
            turn: 0,
        }
    }

    // Provider stub that returns a configurable canned response and records
    // how many times it was called.
    struct StubProvider {
        canned_response: String,
        calls: Mutex<u32>,
    }

    #[async_trait]
    impl Provider for StubProvider {
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[runic_message_types::ToolDefinition],
            _system: &str,
            _resume_session_id: Option<&str>,
        ) -> Result<EventStream, ProviderError> {
            *self.calls.lock().unwrap() += 1;
            let events = vec![
                Ok(StreamEvent::TextDelta(self.canned_response.clone())),
                Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".into()),
                }),
            ];
            Ok(Box::pin(stream::iter(events)))
        }
        fn name(&self) -> &str {
            "stub"
        }
        fn model(&self) -> String {
            "stub-model".into()
        }
        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(StubProvider {
                canned_response: self.canned_response.clone(),
                calls: Mutex::new(0),
            })
        }
    }

    /// Equality on what the provider actually pays attention to —
    /// `Message::user()` stamps creation time, so two builds of the same
    /// history differ in timestamps even though the content is identical.
    fn assert_same_content(left: &[Message], right: &[Message], msg: &str) {
        assert_eq!(left.len(), right.len(), "{msg}: length mismatch");
        for (l, r) in left.iter().zip(right) {
            assert_eq!(l.role, r.role, "{msg}: role mismatch");
            assert_eq!(l.content, r.content, "{msg}: content mismatch");
        }
    }

    fn small_messages() -> Vec<Message> {
        vec![
            Message::user("hi there"),
            Message::assistant_text("hello"),
            Message::user("how are you"),
        ]
    }

    fn big_messages(n: usize, chars_each: usize) -> Vec<Message> {
        (0..n)
            .map(|i| {
                let body = "x".repeat(chars_each);
                if i % 2 == 0 {
                    Message::user(&body)
                } else {
                    Message::assistant_text(&body)
                }
            })
            .collect()
    }

    #[tokio::test]
    async fn under_threshold_does_nothing() {
        let provider = Arc::new(StubProvider {
            canned_response: "summary".into(),
            calls: Mutex::new(0),
        });
        let engine = CompactorEngine::new(Arc::new(NoopEngine), provider.clone())
            .with_token_threshold(100_000);

        let mut messages = small_messages();
        let before = messages.len();
        engine.maybe_compact(&ctx(), &mut messages).await;
        assert_eq!(messages.len(), before, "must not touch messages under threshold");
        assert_eq!(*provider.calls.lock().unwrap(), 0, "must not call provider");
    }

    #[tokio::test]
    async fn over_threshold_compacts_into_one_synthetic_message() {
        let provider = Arc::new(StubProvider {
            canned_response: "Summary: user asked about X, agent did Y.".into(),
            calls: Mutex::new(0),
        });
        let engine = CompactorEngine::new(Arc::new(NoopEngine), provider.clone())
            .with_token_threshold(50)       // very low so we trip easily
            .with_keep_recent(2);

        // 20 messages × ~500 chars each → way over threshold.
        let mut messages = big_messages(20, 500);
        engine.maybe_compact(&ctx(), &mut messages).await;

        // 2 kept + 1 synthetic summary = 3.
        assert_eq!(messages.len(), 3);
        assert_eq!(*provider.calls.lock().unwrap(), 1, "summarizer called exactly once");

        // The first message should be the summary.
        match &messages[0].content[0] {
            ContentBlock::Text { text, .. } => {
                assert!(text.contains("[summary"), "summary marker present: {text}");
                assert!(text.contains("Summary:"), "stub response present: {text}");
            }
            other => panic!("expected Text block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn keep_recent_clamp_when_history_is_short() {
        // If there are fewer messages than `keep_recent`, no compaction
        // happens regardless of threshold.
        let provider = Arc::new(StubProvider {
            canned_response: "x".into(),
            calls: Mutex::new(0),
        });
        let engine = CompactorEngine::new(Arc::new(NoopEngine), provider.clone())
            .with_token_threshold(1)
            .with_keep_recent(20);

        let mut messages = big_messages(5, 5000);
        engine.maybe_compact(&ctx(), &mut messages).await;
        assert_eq!(messages.len(), 5, "history shorter than keep_recent → no-op");
        assert_eq!(*provider.calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn summarization_failure_restores_messages() {
        // Provider that always errors. Compactor must restore messages.
        struct FailingProvider;
        #[async_trait]
        impl Provider for FailingProvider {
            async fn complete(
                &self,
                _messages: &[Message],
                _tools: &[runic_message_types::ToolDefinition],
                _system: &str,
                _resume_session_id: Option<&str>,
            ) -> Result<EventStream, ProviderError> {
                Err(ProviderError::other("nope"))
            }
            fn name(&self) -> &str {
                "fail"
            }
            fn model(&self) -> String {
                "fail".into()
            }
            fn fork(&self) -> Arc<dyn Provider> {
                Arc::new(FailingProvider)
            }
        }

        let engine = CompactorEngine::new(Arc::new(NoopEngine), Arc::new(FailingProvider))
            .with_token_threshold(50)
            .with_keep_recent(2);

        let mut messages = big_messages(10, 500);
        let original_count = messages.len();
        engine.maybe_compact(&ctx(), &mut messages).await;
        assert_eq!(
            messages.len(),
            original_count,
            "failed summarization must not drop messages"
        );
    }

    #[tokio::test]
    async fn delegates_other_methods_to_inner() {
        // Same Marker pattern as the spillover test — confirms only
        // maybe_compact is intercepted; everything else passes through.
        #[derive(Debug)]
        struct Marker;
        #[async_trait]
        impl ContextEngine for Marker {
            async fn assemble_system_prompt(&self, _: &TurnContext<'_>) -> String {
                "FROM_INNER".into()
            }
        }

        let provider = Arc::new(StubProvider {
            canned_response: "".into(),
            calls: Mutex::new(0),
        });
        let engine = CompactorEngine::new(Arc::new(Marker), provider);
        assert_eq!(engine.assemble_system_prompt(&ctx()).await, "FROM_INNER");
    }

    #[tokio::test]
    async fn second_turn_with_same_history_reuses_cached_summary() {
        let provider = Arc::new(StubProvider {
            canned_response: "Summary: stuff happened.".into(),
            calls: Mutex::new(0),
        });
        // Threshold chosen so the post-compaction list (2 kept messages +
        // short summary) lands UNDER it — i.e. realistic proportions.
        let engine = CompactorEngine::new(Arc::new(NoopEngine), provider.clone())
            .with_token_threshold(300)
            .with_keep_recent(2);

        // Turn 1: 20 × 500 chars ≈ 2500 tokens → compacts, one call.
        let mut turn1 = big_messages(20, 500);
        engine.maybe_compact(&ctx(), &mut turn1).await;
        assert_eq!(turn1.len(), 3);
        assert_eq!(*provider.calls.lock().unwrap(), 1);

        // Turn 2: the agent loop rebuilds the SAME original list from
        // events. The cache must kick in with zero provider calls and
        // produce a byte-identical result.
        let mut turn2 = big_messages(20, 500);
        engine.maybe_compact(&ctx(), &mut turn2).await;
        assert_eq!(*provider.calls.lock().unwrap(), 1, "no re-summarization");
        assert_same_content(&turn1, &turn2, "context must be stable across turns");
    }

    #[tokio::test]
    async fn growing_history_extends_summary_incrementally() {
        let provider = Arc::new(StubProvider {
            canned_response: "Summary: stuff happened.".into(),
            calls: Mutex::new(0),
        });
        let engine = CompactorEngine::new(Arc::new(NoopEngine), provider.clone())
            .with_token_threshold(300)
            .with_keep_recent(2);

        // Turn 1: compact 18 of 20 messages.
        let mut turn1 = big_messages(20, 500);
        engine.maybe_compact(&ctx(), &mut turn1).await;
        assert_eq!(*provider.calls.lock().unwrap(), 1);

        // Turn 2: history grew to 30 originals → over threshold again even
        // after the cached summary collapses the first 18. One more call,
        // folding the old summary in; cumulative count reflects originals.
        let mut turn2 = big_messages(30, 500);
        engine.maybe_compact(&ctx(), &mut turn2).await;
        assert_eq!(*provider.calls.lock().unwrap(), 2);
        match &turn2[0].content[0] {
            ContentBlock::Text { text, .. } => {
                assert!(
                    text.contains("28 messages compacted"),
                    "cumulative original count expected: {text}"
                );
            }
            other => panic!("expected Text block, got {other:?}"),
        }

        // Turn 3: same 30 originals → extended cache covers 28, the 2 kept
        // messages fit under threshold → no further calls.
        let mut turn3 = big_messages(30, 500);
        engine.maybe_compact(&ctx(), &mut turn3).await;
        assert_eq!(*provider.calls.lock().unwrap(), 2, "extended cache reused");
        assert_same_content(&turn2, &turn3, "extended context stable across turns");
    }

    #[tokio::test]
    async fn rewritten_history_invalidates_cache() {
        let provider = Arc::new(StubProvider {
            canned_response: "Summary: stuff happened.".into(),
            calls: Mutex::new(0),
        });
        let engine = CompactorEngine::new(Arc::new(NoopEngine), provider.clone())
            .with_token_threshold(300)
            .with_keep_recent(2);

        let mut turn1 = big_messages(20, 500);
        engine.maybe_compact(&ctx(), &mut turn1).await;
        assert_eq!(*provider.calls.lock().unwrap(), 1);

        // A different history (e.g. upstream rewrote a tool result) must
        // not be misrepresented by the stale summary: cache is dropped and
        // a fresh summarization happens.
        let mut different: Vec<Message> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(&"y".repeat(500))
                } else {
                    Message::assistant_text(&"y".repeat(500))
                }
            })
            .collect();
        engine.maybe_compact(&ctx(), &mut different).await;
        assert_eq!(*provider.calls.lock().unwrap(), 2, "stale cache must not be reused");
        assert_eq!(different.len(), 3);
    }

    #[test]
    fn chain_hash_extension_matches_full_recompute() {
        let msgs = big_messages(10, 50);
        let full = chain_hash(CHAIN_SEED, &msgs);
        let prefix = chain_hash(CHAIN_SEED, &msgs[..6]);
        let extended = chain_hash(prefix, &msgs[6..]);
        assert_eq!(full, extended, "chain hash must compose over slices");
    }

    #[test]
    fn estimated_tokens_uses_chars_div_4_heuristic() {
        let engine = CompactorEngine::new(
            Arc::new(NoopEngine),
            Arc::new(StubProvider {
                canned_response: "".into(),
                calls: Mutex::new(0),
            }),
        );
        let msgs = vec![Message::user(&"x".repeat(400))];
        // 400 chars / 4 = 100 tokens (approx).
        assert_eq!(engine.estimated_tokens(&msgs), 100);
    }
}
