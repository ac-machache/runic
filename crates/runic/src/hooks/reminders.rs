use async_trait::async_trait;
use chrono::Utc;
use runic_agent::ReminderQueue;
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_state::{AgentState, SessionEvent};
use runic_types::Message;

pub struct ReminderHook {
    queue: ReminderQueue,
}

impl ReminderHook {
    pub fn new(queue: ReminderQueue) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl WriteHook for ReminderHook {
    fn name(&self) -> &str {
        "reminders"
    }

    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::BeforeModel]
    }

    async fn before_model(&self, state: &mut AgentState) -> HookOutcome {
        let pending = self.queue.drain();
        if pending.is_empty() {
            return HookOutcome::Continue;
        }
        let text = pending
            .iter()
            .map(|r| format!("<system-reminder>\n{r}\n</system-reminder>"))
            .collect::<Vec<_>>()
            .join("\n");
        let run_id = state
            .current_run()
            .map(|r| r.id.clone())
            .unwrap_or_else(|| "reminder".to_string());
        state.push_event(SessionEvent::Message {
            run_id,
            msg: Message::user(text),
            at: Utc::now(),
        });
        HookOutcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AgentState {
        AgentState::new("u1", "s1", "sys")
    }

    #[tokio::test]
    async fn drains_the_queue_into_one_reminder_message() {
        let queue = ReminderQueue::new();
        let hook = ReminderHook::new(queue.clone());
        let mut s = state();

        queue.push("task 'research' finished: found 3 competitors");
        queue.push("task 'audit' failed: timeout");
        hook.before_model(&mut s).await;

        let msgs = s.messages_for_provider();
        assert_eq!(msgs.len(), 1);
        let text = msgs[0].content.text_content();
        assert!(text.contains("<system-reminder>"));
        assert!(text.contains("research"));
        assert!(text.contains("audit"));
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn an_empty_queue_touches_nothing() {
        let hook = ReminderHook::new(ReminderQueue::new());
        let mut s = state();
        hook.before_model(&mut s).await;
        assert!(s.messages_for_provider().is_empty());
        assert!(s.events().is_empty());
    }

    #[tokio::test]
    async fn a_clone_shares_the_queue() {
        let queue = ReminderQueue::new();
        let hook = ReminderHook::new(queue.clone());
        let elsewhere = queue.clone();
        let mut s = state();

        elsewhere.push("pushed from a background task");
        hook.before_model(&mut s).await;
        assert_eq!(s.messages_for_provider().len(), 1);
    }
}
