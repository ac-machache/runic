//! Background memory-review policy and guidance.

use std::sync::atomic::{AtomicU32, Ordering};

/// Guidance handed to the background curator.
pub const MEMORY_REVIEW_GUIDANCE: &str = "\
Review the conversation above and curate memory if anything durable stands out.

Save with the `memory` tool only when it will still matter next week: a user \
preference or correction, an environment fact, a stable convention. Prefer \
declarative facts ('User prefers X') over imperatives ('Always do Y'). Skip \
transient details — task outcomes, PR/issue numbers, commit SHAs, 'phase done'. \
Tidy obviously stale or duplicated entries with `replace`/`remove`. If nothing \
is worth saving, do nothing.";

#[derive(Debug)]
pub struct ReviewScheduler {
    interval: u32,
    since: AtomicU32,
}

impl ReviewScheduler {
    pub fn new(interval: u32) -> Self {
        Self {
            interval,
            since: AtomicU32::new(0),
        }
    }

    /// Whether the nudge is active at all.
    pub fn enabled(&self) -> bool {
        self.interval > 0
    }

    /// Record one completed user turn and return whether review is due.
    pub fn record_turn(&self) -> bool {
        if self.interval == 0 {
            return false;
        }
        let prev = self.since.fetch_add(1, Ordering::SeqCst);
        if prev + 1 >= self.interval {
            self.since.store(0, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    pub fn turns_since(&self) -> u32 {
        self.since.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_every_interval_turns() {
        let s = ReviewScheduler::new(3);
        assert!(s.enabled());
        assert!(!s.record_turn());
        assert!(!s.record_turn());
        assert!(s.record_turn());
        assert_eq!(s.turns_since(), 0);
        assert!(!s.record_turn());
        assert!(!s.record_turn());
        assert!(s.record_turn());
    }

    #[test]
    fn interval_zero_never_fires() {
        let s = ReviewScheduler::new(0);
        assert!(!s.enabled());
        for _ in 0..50 {
            assert!(!s.record_turn());
        }
    }

    #[test]
    fn interval_one_fires_every_turn() {
        let s = ReviewScheduler::new(1);
        assert!(s.record_turn());
        assert!(s.record_turn());
        assert!(s.record_turn());
    }

    #[test]
    fn guidance_is_declarative_not_imperative() {
        assert!(MEMORY_REVIEW_GUIDANCE.contains("declarative"));
        assert!(MEMORY_REVIEW_GUIDANCE.contains("memory"));
    }
}
