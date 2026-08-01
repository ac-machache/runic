use std::sync::Arc;
use std::time::Duration;

use redis::aio::ConnectionManager;
use redis::streams::StreamRangeReply;
use tokio::sync::{broadcast, mpsc};

use super::keys;
use super::wake::Wake;
use super::writer::{self, Write};
use crate::stream::sink::{Replay, RunEvents};
use crate::wire::WireEvent;

const MISSED_NUDGE_GUARD: Duration = Duration::from_secs(5);

pub struct RedisEvents {
    reads: ConnectionManager,
    writes: mpsc::UnboundedSender<Write>,
    wake: Arc<Wake>,
}

impl RedisEvents {
    pub async fn connect(url: &str) -> redis::RedisResult<Arc<Self>> {
        let client = redis::Client::open(url)?;
        let reads = ConnectionManager::new(client.clone()).await?;
        Ok(Arc::new(Self {
            writes: writer::spawn(reads.clone()),
            wake: Wake::spawn(client),
            reads,
        }))
    }

    async fn read(&self, run_id: &str, after: u64) -> redis::RedisResult<Replay> {
        let mut conn = self.reads.clone();
        let (entries, closed): (StreamRangeReply, bool) = redis::pipe()
            .xrange(keys::events(run_id), format!("{}-0", after + 1), "+")
            .exists(keys::closed(run_id))
            .query_async(&mut conn)
            .await?;

        let oldest = entries
            .ids
            .first()
            .and_then(|entry| entry.id.split('-').next()?.parse::<u64>().ok());
        let gap = oldest.is_some_and(|first| first > after + 1);

        let events: Vec<(u64, WireEvent)> = entries
            .ids
            .iter()
            .filter_map(|entry| {
                let seq = entry.id.split('-').next()?.parse().ok()?;
                let payload: String = entry.get("p")?;
                match serde_json::from_str(&payload) {
                    Ok(event) => Some((seq, event)),
                    Err(error) => {
                        tracing::error!(%run_id, seq, %error, "dropping an unreadable run event");
                        None
                    }
                }
            })
            .collect();
        Ok(Replay {
            events,
            gap,
            closed,
        })
    }
}

#[async_trait::async_trait]
impl RunEvents for RedisEvents {
    fn publish(&self, run_id: &str, event: WireEvent) {
        let _ = self.writes.send(Write::Event {
            run_id: run_id.to_string(),
            event,
        });
    }

    fn finish(&self, run_id: &str) {
        let _ = self.writes.send(Write::Finish {
            run_id: run_id.to_string(),
        });
    }

    async fn since(&self, run_id: &str, after: u64) -> Replay {
        let mut nudges = self.wake.watch();
        loop {
            match self.read(run_id, after).await {
                Ok(replay) if settled(&replay) => return replay,
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%run_id, %error, "could not read run events from redis");
                    return Replay {
                        closed: true,
                        ..Replay::default()
                    };
                }
            }
            woken(&mut nudges, run_id).await;
        }
    }
}

fn settled(replay: &Replay) -> bool {
    !replay.events.is_empty() || replay.closed || replay.gap
}

async fn woken(nudges: &mut broadcast::Receiver<String>, run_id: &str) {
    loop {
        match tokio::time::timeout(MISSED_NUDGE_GUARD, nudges.recv()).await {
            Ok(Ok(woken)) if woken != run_id => continue,
            _ => return,
        }
    }
}
