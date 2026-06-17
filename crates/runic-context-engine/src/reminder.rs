//! `ReminderEngine` — surface things that happened *outside* the conversation.
//!
//! This is what gives the agent peripheral vision: between turns, stuff
//! happens (background tasks finish, files change, mcp servers reconnect)
//! and the agent should know without having to ask. Each turn, the engine
//! collects [`AmbientNote`]s from every registered [`Reminder`] and folds
//! them into the system context.
//!
//! Decorator engine: wraps an inner [`ContextEngine`] and intercepts
//! `ambient_notes` to append its own contributions on top of whatever the
//! inner produces. Notes carrying a `dedup_key` are tracked in-process so
//! the same reminder doesn't fire twice across turns.
//!
//! ```text
//! ReminderEngine
//!   ├─ inner: Arc<dyn ContextEngine>          ← the rest of the pipeline
//!   ├─ reminders: Vec<Arc<dyn Reminder>>      ← registered sources
//!   └─ seen: Mutex<HashSet<String>>           ← dedup state (in-memory)
//! ```
//!
//! Ships one built-in reminder: [`BackgroundTaskReminder`], which announces
//! background-task completions / failures / cancellations once each. Other
//! reminders are purely additive — implement [`Reminder`] and pass via
//! [`ReminderEngine::with_reminder`].

use async_trait::async_trait;
use runic_message_types::Message;
use runic_tool_core::{BackgroundManager, TaskStatusView};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::{AmbientNote, ContextEngine, TurnContext};

// ─── The trait ──────────────────────────────────────────────────────────────

/// A pluggable source of ambient context. Implementors watch some
/// subsystem (background tasks, file changes, etc.) and report what's
/// new each turn as [`AmbientNote`]s.
///
/// Notes with a `dedup_key` are emitted at most once across the lifetime
/// of the enclosing [`ReminderEngine`].
#[async_trait]
pub trait Reminder: Send + Sync + std::fmt::Debug {
    /// Short name for debugging / diagnostics.
    fn name(&self) -> &str;

    /// Collect everything this reminder wants the model to know about
    /// this turn. May return empty.
    async fn collect(&self, ctx: &TurnContext<'_>) -> Vec<AmbientNote>;
}

// ─── The engine ─────────────────────────────────────────────────────────────

pub struct ReminderEngine {
    inner: Arc<dyn ContextEngine>,
    reminders: Vec<Arc<dyn Reminder>>,
    seen: Mutex<HashSet<String>>,
}

impl ReminderEngine {
    pub fn new(inner: Arc<dyn ContextEngine>) -> Self {
        Self {
            inner,
            reminders: Vec::new(),
            seen: Mutex::new(HashSet::new()),
        }
    }

    pub fn with_reminder<R>(mut self, reminder: R) -> Self
    where
        R: Reminder + 'static,
    {
        self.reminders.push(Arc::new(reminder));
        self
    }

    /// Names of registered reminders, in registration order.
    pub fn reminder_names(&self) -> Vec<&str> {
        self.reminders.iter().map(|r| r.name()).collect()
    }

    pub fn len(&self) -> usize {
        self.reminders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.reminders.is_empty()
    }
}

impl std::fmt::Debug for ReminderEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReminderEngine")
            .field("reminders", &self.reminder_names())
            .finish()
    }
}

#[async_trait]
impl ContextEngine for ReminderEngine {
    async fn assemble_system_prompt(&self, ctx: &TurnContext<'_>) -> String {
        self.inner.assemble_system_prompt(ctx).await
    }

    async fn process_user_input(&self, ctx: &TurnContext<'_>, msg: Message) -> Message {
        self.inner.process_user_input(ctx, msg).await
    }

    async fn maybe_compact(&self, ctx: &TurnContext<'_>, messages: &mut Vec<Message>) {
        self.inner.maybe_compact(ctx, messages).await;
    }

    async fn ambient_notes(&self, ctx: &TurnContext<'_>) -> Vec<AmbientNote> {
        // Start with whatever the inner engine produces (CompositeEngine,
        // others) so existing ambient notes still flow through.
        let mut out = self.inner.ambient_notes(ctx).await;

        for reminder in &self.reminders {
            let candidates = reminder.collect(ctx).await;
            let mut seen = self.seen.lock().await;
            for note in candidates {
                if let Some(key) = &note.dedup_key {
                    if seen.contains(key) {
                        continue;
                    }
                    seen.insert(key.clone());
                }
                out.push(note);
            }
        }
        out
    }
}

// ─── BackgroundTaskReminder ─────────────────────────────────────────────────

/// Announce when a background task transitions out of `Running`.
///
/// Watches the configured [`BackgroundManager`]; each turn, scans every
/// known task id and reports the ones that are now `Done` (with result),
/// `Cancelled`, or never-existed-but-was-promised. Uses the task id as
/// the dedup key so each completion is announced exactly once.
#[derive(Debug)]
pub struct BackgroundTaskReminder {
    manager: Arc<BackgroundManager>,
}

impl BackgroundTaskReminder {
    pub fn new(manager: Arc<BackgroundManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Reminder for BackgroundTaskReminder {
    fn name(&self) -> &str {
        "background-tasks"
    }

    async fn collect(&self, _ctx: &TurnContext<'_>) -> Vec<AmbientNote> {
        let mut notes = Vec::new();
        for id in self.manager.task_ids() {
            let Some(status) = self.manager.check(&id) else {
                continue;
            };
            match status {
                TaskStatusView::Running { .. } => {
                    // Still running — nothing to remind about yet.
                }
                TaskStatusView::Done {
                    tool_name,
                    result,
                    duration_ms,
                } => {
                    let outcome = if result.is_error { "errored" } else { "completed" };
                    let preview = truncate(&result.content, 240);
                    notes.push(AmbientNote {
                        source: format!("background-task:{id}"),
                        content: format!(
                            "task {id} ({tool_name}) {outcome} in {duration_ms}ms — {preview}"
                        ),
                        dedup_key: Some(format!("bg-done:{id}")),
                    });
                }
                TaskStatusView::Cancelled {
                    tool_name,
                    duration_ms,
                } => {
                    notes.push(AmbientNote {
                        source: format!("background-task:{id}"),
                        content: format!(
                            "task {id} ({tool_name}) was cancelled after {duration_ms}ms"
                        ),
                        dedup_key: Some(format!("bg-cancelled:{id}")),
                    });
                }
            }
        }
        notes
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoopEngine;
    use runic_tool_core::ToolResult;
    use std::time::Duration;

    fn ctx<'a>() -> TurnContext<'a> {
        TurnContext {
            base_system_prompt: "",
            messages: &[],
            run_id: "run-1",
            turn: 0,
            config: crate::empty_config(),
        }
    }

    // ─── ReminderEngine plumbing ────────────────────────────────────────────

    #[derive(Debug, Clone)]
    struct FixedReminder {
        name_str: String,
        notes: Vec<AmbientNote>,
    }
    #[async_trait]
    impl Reminder for FixedReminder {
        fn name(&self) -> &str {
            &self.name_str
        }
        async fn collect(&self, _ctx: &TurnContext<'_>) -> Vec<AmbientNote> {
            self.notes.clone()
        }
    }

    fn note(source: &str, content: &str, dedup_key: Option<&str>) -> AmbientNote {
        AmbientNote {
            source: source.into(),
            content: content.into(),
            dedup_key: dedup_key.map(String::from),
        }
    }

    #[tokio::test]
    async fn empty_engine_passes_through_inner_notes() {
        let engine = ReminderEngine::new(Arc::new(NoopEngine));
        assert!(engine.ambient_notes(&ctx()).await.is_empty());
        assert!(engine.is_empty());
        assert_eq!(engine.len(), 0);
    }

    #[tokio::test]
    async fn registered_reminders_contribute_notes() {
        let engine = ReminderEngine::new(Arc::new(NoopEngine))
            .with_reminder(FixedReminder {
                name_str: "alpha".into(),
                notes: vec![note("a", "from alpha", None)],
            })
            .with_reminder(FixedReminder {
                name_str: "beta".into(),
                notes: vec![note("b", "from beta", None)],
            });

        let notes = engine.ambient_notes(&ctx()).await;
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].content, "from alpha");
        assert_eq!(notes[1].content, "from beta");
    }

    #[tokio::test]
    async fn notes_with_dedup_key_only_emit_once_across_turns() {
        let engine = ReminderEngine::new(Arc::new(NoopEngine)).with_reminder(FixedReminder {
            name_str: "x".into(),
            notes: vec![note("s", "fire-once", Some("k1"))],
        });

        let turn1 = engine.ambient_notes(&ctx()).await;
        let turn2 = engine.ambient_notes(&ctx()).await;
        let turn3 = engine.ambient_notes(&ctx()).await;

        assert_eq!(turn1.len(), 1);
        assert!(turn2.is_empty());
        assert!(turn3.is_empty());
    }

    #[tokio::test]
    async fn notes_without_dedup_key_emit_every_turn() {
        let engine = ReminderEngine::new(Arc::new(NoopEngine)).with_reminder(FixedReminder {
            name_str: "x".into(),
            notes: vec![note("s", "always", None)],
        });

        let t1 = engine.ambient_notes(&ctx()).await;
        let t2 = engine.ambient_notes(&ctx()).await;
        assert_eq!(t1.len(), 1);
        assert_eq!(t2.len(), 1);
    }

    #[tokio::test]
    async fn dedup_state_is_per_engine_not_per_reminder() {
        // Two reminders that both want to emit the SAME dedup key —
        // only the first one through wins.
        let engine = ReminderEngine::new(Arc::new(NoopEngine))
            .with_reminder(FixedReminder {
                name_str: "first".into(),
                notes: vec![note("a", "from first", Some("shared"))],
            })
            .with_reminder(FixedReminder {
                name_str: "second".into(),
                notes: vec![note("b", "from second", Some("shared"))],
            });

        let notes = engine.ambient_notes(&ctx()).await;
        assert_eq!(notes.len(), 1, "dedup key collides → only first emitted");
        assert_eq!(notes[0].content, "from first");
    }

    #[tokio::test]
    async fn inner_notes_are_preserved_alongside_reminder_notes() {
        #[derive(Debug)]
        struct InnerWithNotes;
        #[async_trait]
        impl ContextEngine for InnerWithNotes {
            async fn ambient_notes(&self, _: &TurnContext<'_>) -> Vec<AmbientNote> {
                vec![note("inner", "from inner engine", None)]
            }
        }

        let engine = ReminderEngine::new(Arc::new(InnerWithNotes)).with_reminder(FixedReminder {
            name_str: "r".into(),
            notes: vec![note("r", "from reminder", None)],
        });

        let notes = engine.ambient_notes(&ctx()).await;
        assert_eq!(notes.len(), 2);
        // Inner first, reminder second.
        assert_eq!(notes[0].content, "from inner engine");
        assert_eq!(notes[1].content, "from reminder");
    }

    #[tokio::test]
    async fn reminder_names_are_returned_in_registration_order() {
        let engine = ReminderEngine::new(Arc::new(NoopEngine))
            .with_reminder(FixedReminder {
                name_str: "first".into(),
                notes: vec![],
            })
            .with_reminder(FixedReminder {
                name_str: "second".into(),
                notes: vec![],
            });
        assert_eq!(engine.reminder_names(), vec!["first", "second"]);
        assert_eq!(engine.len(), 2);
    }

    // ─── BackgroundTaskReminder ─────────────────────────────────────────────

    #[tokio::test]
    async fn background_reminder_announces_completed_tasks_once() {
        let manager = Arc::new(BackgroundManager::new());

        // Quick task that completes within a few ms.
        let id = manager.start("instant".into(), async { ToolResult::ok("hello") });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let reminder = BackgroundTaskReminder::new(manager.clone());
        let engine = ReminderEngine::new(Arc::new(NoopEngine)).with_reminder(reminder);

        let t1 = engine.ambient_notes(&ctx()).await;
        assert_eq!(t1.len(), 1);
        assert!(t1[0].content.contains(&id));
        assert!(t1[0].content.contains("instant"));
        assert!(t1[0].content.contains("hello"));
        assert!(t1[0].content.contains("completed"));

        // Second turn — dedup should suppress.
        let t2 = engine.ambient_notes(&ctx()).await;
        assert!(t2.is_empty(), "completion should fire exactly once");
    }

    #[tokio::test]
    async fn background_reminder_marks_errored_tasks_distinctly() {
        let manager = Arc::new(BackgroundManager::new());
        let id = manager.start("oops".into(), async { ToolResult::error("nope") });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let engine = ReminderEngine::new(Arc::new(NoopEngine))
            .with_reminder(BackgroundTaskReminder::new(manager.clone()));

        let notes = engine.ambient_notes(&ctx()).await;
        assert_eq!(notes.len(), 1);
        assert!(notes[0].content.contains(&id));
        assert!(notes[0].content.contains("errored"));
    }

    #[tokio::test]
    async fn background_reminder_announces_cancellation_once() {
        let manager = Arc::new(BackgroundManager::new());
        let id = manager.start("long".into(), async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            ToolResult::ok("never")
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        manager.abort(&id);

        let engine = ReminderEngine::new(Arc::new(NoopEngine))
            .with_reminder(BackgroundTaskReminder::new(manager.clone()));

        let t1 = engine.ambient_notes(&ctx()).await;
        assert_eq!(t1.len(), 1);
        assert!(t1[0].content.contains(&id));
        assert!(t1[0].content.contains("cancelled"));

        let t2 = engine.ambient_notes(&ctx()).await;
        assert!(t2.is_empty(), "cancellation announced once");
    }

    #[tokio::test]
    async fn background_reminder_says_nothing_while_tasks_still_running() {
        let manager = Arc::new(BackgroundManager::new());
        let _id = manager.start("slow".into(), async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            ToolResult::ok("never reached")
        });
        // Do NOT wait — task is still running.

        let engine = ReminderEngine::new(Arc::new(NoopEngine))
            .with_reminder(BackgroundTaskReminder::new(manager.clone()));
        let notes = engine.ambient_notes(&ctx()).await;
        assert!(notes.is_empty(), "running tasks should not produce reminders");
    }

    #[test]
    fn truncate_keeps_short_strings_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn truncate_appends_ellipsis_to_long_strings() {
        let long = "x".repeat(300);
        let out = truncate(&long, 50);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 51);
    }
}
