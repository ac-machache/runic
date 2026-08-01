use redis::aio::ConnectionManager;
use tokio::sync::mpsc;

use super::keys;
use crate::stream::sink::{MAX_BYTES, weight};
use crate::wire::WireEvent;

pub enum Write {
    Event { run_id: String, event: WireEvent },
    Finish { run_id: String },
}

pub fn spawn(redis: ConnectionManager) -> mpsc::UnboundedSender<Write> {
    let (outbox, mut inbox) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let publish = redis::Script::new(include_str!("publish.lua"));
        let mut conn = redis;
        while let Some(write) = inbox.recv().await {
            let run_id = match &write {
                Write::Event { run_id, .. } | Write::Finish { run_id } => run_id.clone(),
            };
            if let Err(error) = apply(&mut conn, &publish, write).await {
                tracing::warn!(%run_id, %error, "could not publish run events to redis");
            }
        }
    });
    outbox
}

async fn apply(
    conn: &mut ConnectionManager,
    publish: &redis::Script,
    write: Write,
) -> redis::RedisResult<()> {
    match write {
        Write::Event { run_id, event } => {
            let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            publish
                .key(keys::events(&run_id))
                .key(keys::sequence(&run_id))
                .key(keys::bytes(&run_id))
                .arg(payload)
                .arg(weight(&event))
                .arg(MAX_BYTES)
                .arg(keys::RETENTION.as_secs())
                .arg(keys::CHANNEL)
                .arg(&run_id)
                .invoke_async::<i64>(conn)
                .await?;
            Ok(())
        }
        Write::Finish { run_id } => {
            redis::pipe()
                .atomic()
                .del(keys::events(&run_id))
                .del(keys::sequence(&run_id))
                .del(keys::bytes(&run_id))
                .set_ex(keys::closed(&run_id), 1, keys::RETENTION.as_secs())
                .publish(keys::CHANNEL, &run_id)
                .query_async::<()>(conn)
                .await
        }
    }
}
