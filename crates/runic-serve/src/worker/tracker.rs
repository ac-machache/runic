use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::Notify;

pub struct Tracker {
    min_runs: usize,
    max_runs: usize,
    running: AtomicUsize,
    live: Mutex<HashSet<String>>,
    ready: Notify,
}

impl Tracker {
    pub fn new(min_runs: usize, max_runs: usize) -> Self {
        Self {
            min_runs,
            max_runs,
            running: AtomicUsize::new(0),
            live: Mutex::new(HashSet::new()),
            ready: Notify::new(),
        }
    }

    pub fn next_batch_size(&self) -> Option<usize> {
        let running = self.running.load(Ordering::SeqCst);
        match running < self.min_runs {
            true => Some(self.max_runs - running),
            false => None,
        }
    }

    pub fn started(&self, run_id: &str) {
        self.running.fetch_add(1, Ordering::SeqCst);
        self.live
            .lock()
            .expect("tracker poisoned")
            .insert(run_id.to_string());
    }

    pub fn finished(&self, run_id: &str) {
        let before = self.running.fetch_sub(1, Ordering::SeqCst);
        self.live.lock().expect("tracker poisoned").remove(run_id);
        if before == self.min_runs {
            self.ready.notify_one();
        }
    }

    pub fn live_ids(&self) -> Vec<String> {
        self.live
            .lock()
            .expect("tracker poisoned")
            .iter()
            .cloned()
            .collect()
    }

    pub fn wake(&self) {
        self.ready.notify_one();
    }

    pub fn woken(&self) -> tokio::sync::futures::Notified<'_> {
        self.ready.notified()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_up_to_max_only_once_below_min() {
        let tracker = Tracker::new(2, 8);
        assert_eq!(tracker.next_batch_size(), Some(8));

        tracker.started("run-a");
        assert_eq!(tracker.next_batch_size(), Some(7));

        tracker.started("run-b");
        assert_eq!(
            tracker.next_batch_size(),
            None,
            "at min capacity the worker should stop asking for more"
        );

        tracker.finished("run-b");
        assert_eq!(tracker.next_batch_size(), Some(7));
    }

    #[test]
    fn live_ids_follow_what_is_running() {
        let tracker = Tracker::new(1, 4);
        tracker.started("run-a");
        tracker.started("run-b");

        let mut ids = tracker.live_ids();
        ids.sort();
        assert_eq!(ids, ["run-a", "run-b"]);

        tracker.finished("run-a");
        assert_eq!(tracker.live_ids(), ["run-b"]);
    }
}
