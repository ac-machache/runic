use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::oneshot;

pub const CHANNEL: &str = "runic_run_done";

const REATTACH_AFTER: Duration = Duration::from_secs(1);

type Waiters = HashMap<String, Vec<(u64, oneshot::Sender<()>)>>;

#[derive(Clone, Default)]
pub struct Completions {
    waiting: Arc<Mutex<Waiters>>,
    next: Arc<AtomicU64>,
}

pub struct Ticket {
    run_id: String,
    id: u64,
    signal: oneshot::Receiver<()>,
    completions: Completions,
}

impl Completions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ticket(&self, run_id: &str) -> Ticket {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, signal) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap()
            .entry(run_id.to_string())
            .or_default()
            .push((id, tx));
        Ticket {
            run_id: run_id.to_string(),
            id,
            signal,
            completions: self.clone(),
        }
    }

    fn wake(&self, run_id: &str) {
        let Some(waiters) = self.waiting.lock().unwrap().remove(run_id) else {
            return;
        };
        for (_, sender) in waiters {
            let _ = sender.send(());
        }
    }

    fn release(&self, run_id: &str, id: u64) {
        let mut waiting = self.waiting.lock().unwrap();
        let Some(waiters) = waiting.get_mut(run_id) else {
            return;
        };
        waiters.retain(|(waiter, _)| *waiter != id);
        if waiters.is_empty() {
            waiting.remove(run_id);
        }
    }

    pub fn watch(&self, pool: PgPool) -> tokio::task::JoinHandle<()> {
        let completions = self.clone();
        tokio::spawn(async move {
            loop {
                match attach(&pool).await {
                    Ok(mut listener) => {
                        tracing::info!(channel = CHANNEL, "completion listener attached");
                        while let Ok(note) = listener.recv().await {
                            completions.wake(note.payload());
                        }
                        tracing::warn!("completion listener dropped, reattaching");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "completion listener could not attach");
                    }
                }
                tokio::time::sleep(REATTACH_AFTER).await;
            }
        })
    }
}

async fn attach(pool: &PgPool) -> Result<PgListener, sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    Ok(listener)
}

impl Ticket {
    pub async fn settled(&mut self, budget: Duration) {
        let _ = tokio::time::timeout(budget, &mut self.signal).await;
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.completions.release(&self.run_id, self.id);
    }
}
