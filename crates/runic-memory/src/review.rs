//! Background memory-review — hermes's "memory nudge". Every N user turns the
//! agent should spend a cheap off-loop pass curating memory (saving durable
//! facts it noticed, tidying stale entries) instead of relying on the model to
//! remember to call the tool mid-task.
//!
//! This module owns only the **policy**: a turn counter that says *when* a
//! review is due, plus the guidance text the review runs with. The actual
//! spawn — a memory-and-skills-only sub-agent sharing the same
//! [`BoundedMemoryStore`](crate::store::BoundedMemoryStore) — belongs to the
//! wiring layer (it needs the agent loop / `delegate`, which must not be a
//! dependency of this crate). The wiring calls [`ReviewScheduler::record_turn`]
//! after each user turn and, when it returns `true`, delegates a curator agent
//! seeded with [`MEMORY_REVIEW_GUIDANCE`].

use std::sync::atomic::{AtomicU32, Ordering};

/// Guidance handed to the background curator sub-agent (hermes
/// `MEMORY_REVIEW_GUIDANCE`).
pub const MEMORY_REVIEW_GUIDANCE: &str = "\
Review the conversation above and curate memory if anything durable stands out.

Save with the `memory` tool only when it will still matter next week: a user \
preference or correction, an environment fact, a stable convention. Prefer \
declarative facts ('User prefers X') over imperatives ('Always do Y'). Skip \
transient details — task outcomes, PR/issue numbers, commit SHAs, 'phase done'. \
Tidy obviously stale or duplicated entries with `replace`/`remove`. If nothing \
is worth saving, do nothing.";

/// Turn counter governing when a background review fires. Cheap, lock-free, and
/// shareable across the agent's turn boundary.
#[derive(Debug)]
pub struct ReviewScheduler {
    /// Turns between reviews. `0` disables the nudge entirely.
    interval: u32,
    since: AtomicU32,
}

impl ReviewScheduler {
    /// `interval` turns between reviews (`0` = disabled, matching
    /// [`MemoryConfig::nudge_interval`](crate::config::MemoryConfig)).
    pub fn new(interval: u32) -> Self {
        Self { interval, since: AtomicU32::new(0) }
    }

    /// Whether the nudge is active at all.
    pub fn enabled(&self) -> bool {
        self.interval > 0
    }

    /// Record one completed user turn. Returns `true` exactly when a review is
    /// due, resetting the counter in the same step. Always `false` when
    /// disabled.
    pub fn record_turn(&self) -> bool {
        if self.interval == 0 {
            return false;
        }
        // fetch_add returns the value *before* the increment.
        let prev = self.since.fetch_add(1, Ordering::SeqCst);
        if prev + 1 >= self.interval {
            self.since.store(0, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Turns elapsed since the last review (for diagnostics/UX).
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
        assert!(!s.record_turn()); // 1
        assert!(!s.record_turn()); // 2
        assert!(s.record_turn()); // 3 → due, resets
        assert_eq!(s.turns_since(), 0);
        assert!(!s.record_turn()); // 1
        assert!(!s.record_turn()); // 2
        assert!(s.record_turn()); // 3 → due again
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
