use std::time::Duration;

pub const CHANNEL: &str = "runic:events";
pub const RETENTION: Duration = Duration::from_secs(900);

pub fn events(run_id: &str) -> String {
    format!("runic:run:{run_id}:events")
}

pub fn sequence(run_id: &str) -> String {
    format!("runic:run:{run_id}:seq")
}

pub fn bytes(run_id: &str) -> String {
    format!("runic:run:{run_id}:bytes")
}

pub fn closed(run_id: &str) -> String {
    format!("runic:run:{run_id}:closed")
}
