use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use runic::CancelToken;
use tokio::sync::{Notify, mpsc};

pub struct Controls {
    pub cancel: CancelToken,
    pub steer: mpsc::UnboundedSender<String>,
}

pub struct Tracker {
    min_runs: usize,
    max_runs: usize,
    running: AtomicUsize,
    live: Mutex<HashMap<String, Controls>>,
    ready: Notify,
}

impl Tracker {
    pub fn new(min_runs: usize, max_runs: usize) -> Self {
        Self {
            min_runs,
            max_runs,
            running: AtomicUsize::new(0),
            live: Mutex::new(HashMap::new()),
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

    pub fn started(&self, run_id: &str) -> (CancelToken, mpsc::UnboundedReceiver<String>) {
        let (steer, steering) = mpsc::unbounded_channel();
        let cancel = CancelToken::new();
        self.running.fetch_add(1, Ordering::SeqCst);
        self.live.lock().expect("tracker poisoned").insert(
            run_id.to_string(),
            Controls {
                cancel: cancel.clone(),
                steer,
            },
        );
        (cancel, steering)
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
            .keys()
            .cloned()
            .collect()
    }

    pub fn holds(&self, run_id: &str) -> bool {
        self.live
            .lock()
            .expect("tracker poisoned")
            .contains_key(run_id)
    }

    pub fn cancel(&self, run_id: &str) -> bool {
        match self.live.lock().expect("tracker poisoned").get(run_id) {
            Some(controls) => {
                controls.cancel.cancel();
                true
            }
            None => false,
        }
    }

    pub fn steer(&self, run_id: &str, text: &str) -> bool {
        match self.live.lock().expect("tracker poisoned").get(run_id) {
            Some(controls) => controls.steer.send(text.to_string()).is_ok(),
            None => false,
        }
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

    #[test]
    fn signals_only_reach_runs_this_worker_holds() {
        let tracker = Tracker::new(1, 4);
        let (cancel, mut inbox) = tracker.started("run-a");

        assert!(tracker.steer("run-a", "focus on the tests"));
        assert_eq!(inbox.try_recv().ok().as_deref(), Some("focus on the tests"));

        assert!(tracker.cancel("run-a"));
        assert!(cancel.is_cancelled());

        assert!(!tracker.cancel("run-elsewhere"));
        assert!(!tracker.steer("run-elsewhere", "ignored"));

        tracker.finished("run-a");
        assert!(!tracker.steer("run-a", "too late"));
    }
}
