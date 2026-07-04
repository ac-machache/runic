use async_trait::async_trait;
use chrono::Utc;
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_state::{AgentState, SessionEvent, TaskStatus};
use runic_types::Message;

#[derive(Default)]
pub struct TaskReminder;

impl TaskReminder {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl WriteHook for TaskReminder {
    fn name(&self) -> &str {
        "task-reminder"
    }

    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::BeforeModel]
    }

    async fn before_model(&self, state: &mut AgentState) -> HookOutcome {
        let mut due: Vec<_> = state
            .tasks
            .values()
            .filter(|t| {
                t.status != TaskStatus::Running && state.get(&notified_key(&t.task_id)).is_none()
            })
            .cloned()
            .collect();
        if due.is_empty() {
            return HookOutcome::Continue;
        }
        due.sort_by(|a, b| a.spawned_at.cmp(&b.spawned_at));

        let notes: Vec<String> = due
            .iter()
            .map(|t| {
                let body = match t.status {
                    TaskStatus::Completed => format!(
                        "background task {} ({}) completed:\n{}",
                        t.task_id,
                        t.agent,
                        t.result.clone().unwrap_or_default()
                    ),
                    TaskStatus::Failed => format!(
                        "background task {} ({}) failed: {}",
                        t.task_id,
                        t.agent,
                        t.result.clone().unwrap_or_default()
                    ),
                    TaskStatus::Cancelled => {
                        format!("background task {} ({}) was cancelled", t.task_id, t.agent)
                    }
                    TaskStatus::Running => unreachable!(),
                };
                format!("<system-reminder>\n{body}\n</system-reminder>")
            })
            .collect();

        for t in &due {
            state.update(notified_key(&t.task_id), serde_json::json!(true));
        }

        let run_id = state
            .current_run()
            .map(|r| r.id.clone())
            .unwrap_or_else(|| "task-reminder".to_string());
        state.push_event(SessionEvent::Message {
            run_id,
            msg: Message::user(notes.join("\n")),
            at: Utc::now(),
        });
        HookOutcome::Continue
    }
}

fn notified_key(task_id: &str) -> String {
    format!("task-reminder/notified/{task_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_state::TaskRecord;

    fn finished(id: &str, status: TaskStatus, result: Option<&str>) -> TaskRecord {
        TaskRecord {
            task_id: id.into(),
            agent: "scout".into(),
            prompt: "dig".into(),
            status,
            result: result.map(str::to_string),
            spawned_at: Utc::now(),
            finished_at: Some(Utc::now()),
        }
    }

    #[tokio::test]
    async fn reminds_once_per_finished_task() {
        let hook = TaskReminder::new();
        let mut s = AgentState::new("u1", "s1", "sys");
        s.tasks.insert(
            "t1".into(),
            finished("t1", TaskStatus::Completed, Some("gold")),
        );
        s.tasks.insert(
            "t2".into(),
            finished("t2", TaskStatus::Failed, Some("timeout")),
        );

        hook.before_model(&mut s).await;
        let text = {
            let msgs = s.messages_for_provider();
            assert_eq!(msgs.len(), 1);
            msgs[0].content.text_content()
        };
        assert!(text.contains("t1") && text.contains("gold"));
        assert!(text.contains("t2") && text.contains("timeout"));
        assert_eq!(s.get(&notified_key("t1")), Some(&serde_json::json!(true)));
        assert_eq!(s.get(&notified_key("t2")), Some(&serde_json::json!(true)));

        hook.before_model(&mut s).await;
        assert_eq!(s.messages_for_provider().len(), 1);
    }

    #[tokio::test]
    async fn running_tasks_are_not_reminded() {
        let hook = TaskReminder::new();
        let mut s = AgentState::new("u1", "s1", "sys");
        let mut record = finished("t1", TaskStatus::Running, None);
        record.finished_at = None;
        s.tasks.insert("t1".into(), record);

        hook.before_model(&mut s).await;
        assert!(s.messages_for_provider().is_empty());
        assert!(s.get(&notified_key("t1")).is_none());
    }
}
