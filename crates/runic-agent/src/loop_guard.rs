//! `loop_guard` — runaway-loop protection (ported from OpenFang's `loop_guard`,
//! including its battle-tested test suite — see the `ported_from_openfang`
//! module below).
//!
//! A turn loop's worst failure mode is silent: the model calling the same tool
//! forever, or oscillating between two tools, burning tokens without progress.
//! `max_turns` is a blunt backstop; this guard catches the patterns far sooner.
//!
//! Detections, cheapest first:
//! - **per-call repeat**: identical `(tool, args)` calls — warn at
//!   `warn_threshold`, block at `block_threshold` (thresholds fire *at* the
//!   count, i.e. `>=`, matching OpenFang).
//! - **warn→block upgrade**: after `max_warnings_per_call` warnings for the
//!   same call, block instead.
//! - **ping-pong**: A-B-A-B (period 2) or A-B-C-… (period 3) cycles in the
//!   recent-call window — block at `ping_pong_min_repeats` full cycles.
//! - **outcome-aware**: when an identical call keeps producing an identical
//!   result, escalate (the approach demonstrably isn't working).
//! - **poll relaxation**: status/poll/wait tools legitimately repeat, so their
//!   thresholds are multiplied by `poll_multiplier`.
//! - **global circuit breaker**: total tool calls in a run.

use std::collections::{HashMap, HashSet, VecDeque};

use runic_types::ToolCall;
use serde_json::Value;

/// Size of the recent-call ring buffer used for ping-pong detection.
const RECENT_WINDOW: usize = 30;
/// Result prefix length folded into the outcome hash.
const OUTCOME_RESULT_PREFIX: usize = 512;

/// What the guard decides for a proposed tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Run the tool normally.
    Allow,
    /// Run the tool, but append this nudge to its result.
    Warn(String),
    /// Skip the tool; feed this message back to the model as an error result.
    Block(String),
    /// Abort the whole run — the model is stuck.
    CircuitBreak(String),
}

/// Thresholds for the guard.
#[derive(Debug, Clone)]
pub struct LoopGuardConfig {
    /// Identical calls before warning (fires at this count).
    pub warn_threshold: u32,
    /// Identical calls before blocking (fires at this count).
    pub block_threshold: u32,
    /// Warnings for one call before upgrading to a block.
    pub max_warnings_per_call: u32,
    /// Multiplier applied to thresholds for poll-style tools.
    pub poll_multiplier: u32,
    /// Identical (call, result) pairs before warning.
    pub outcome_warn_threshold: u32,
    /// Identical (call, result) pairs before blocking the next such call.
    pub outcome_block_threshold: u32,
    /// Full ping-pong cycles before blocking.
    pub ping_pong_min_repeats: u32,
    /// Total tool calls in a run before the circuit breaker trips.
    pub global_circuit_breaker: u32,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            warn_threshold: 3,
            block_threshold: 5,
            max_warnings_per_call: 3,
            poll_multiplier: 3,
            outcome_warn_threshold: 2,
            outcome_block_threshold: 3,
            ping_pong_min_repeats: 3,
            // OpenFang defaults to 30 and scales it up at runtime; we keep a
            // higher flat cap since `max_turns` is our separate hard backstop
            // and a turn may legitimately issue several (parallel) tool calls.
            global_circuit_breaker: 100,
        }
    }
}

/// A snapshot of guard state (for debugging / tests).
#[derive(Debug, Clone)]
pub struct LoopGuardStats {
    pub total_calls: u32,
    pub distinct_calls: usize,
    pub ping_pong_detected: bool,
}

/// Per-run loop guard. Cheap: hashes `(name, args)` and counts.
#[derive(Debug, Default)]
pub struct LoopGuard {
    config: LoopGuardConfig,
    total_calls: u32,
    call_counts: HashMap<String, u32>,
    warnings_emitted: HashMap<String, u32>,
    recent_calls: VecDeque<String>,
    outcome_counts: HashMap<String, u32>,
    blocked_outcomes: HashSet<String>,
    ping_pong_detected: bool,
}

impl LoopGuard {
    /// A guard with the given thresholds (OpenFang-compatible constructor).
    pub fn new(config: LoopGuardConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// Alias of [`LoopGuard::new`] for call sites that read better.
    pub fn with_config(config: LoopGuardConfig) -> Self {
        Self::new(config)
    }

    /// Reset between runs.
    pub fn reset(&mut self) {
        self.total_calls = 0;
        self.call_counts.clear();
        self.warnings_emitted.clear();
        self.recent_calls.clear();
        self.outcome_counts.clear();
        self.blocked_outcomes.clear();
        self.ping_pong_detected = false;
    }

    /// Check a proposed tool call, recording it for future checks.
    pub fn check(&mut self, call: &ToolCall) -> Verdict {
        self.check_parts(&call.name, &call.input)
    }

    /// Check by `(name, params)` — the OpenFang-shaped entry point.
    pub fn check_parts(&mut self, name: &str, params: &Value) -> Verdict {
        self.total_calls += 1;
        if self.total_calls > self.config.global_circuit_breaker {
            return Verdict::CircuitBreak(format!(
                "exceeded {} total tool calls in one run",
                self.config.global_circuit_breaker
            ));
        }

        let key = call_key(name, params);

        if self.recent_calls.len() == RECENT_WINDOW {
            self.recent_calls.pop_front();
        }
        self.recent_calls.push_back(key.clone());

        // Outcome-aware: a prior identical (call, result) streak armed a block.
        if self.blocked_outcomes.contains(&key) {
            return Verdict::Block(format!(
                "tool '{name}' keeps returning identical results for the same \
                 input — that approach isn't working; try something different"
            ));
        }

        let count = {
            let c = self.call_counts.entry(key.clone()).or_insert(0);
            *c += 1;
            *c
        };
        let mult = if Self::is_poll_call(name, params) {
            self.config.poll_multiplier.max(1)
        } else {
            1
        };
        let warn_at = self.config.warn_threshold * mult;
        let block_at = self.config.block_threshold * mult;

        if count >= block_at {
            return Verdict::Block(format!(
                "tool '{name}' called {count} times with identical arguments — \
                 stop repeating it and try a different approach"
            ));
        }

        // Ping-pong: oscillation the per-call counter can't see.
        let cycles = self.max_ping_pong_cycles();
        if cycles >= self.config.ping_pong_min_repeats {
            self.ping_pong_detected = true;
            return Verdict::Block(format!(
                "Ping-pong loop detected ({cycles}× cycle) — break the loop"
            ));
        }

        if count >= warn_at {
            let warns = {
                let w = self.warnings_emitted.entry(key).or_insert(0);
                *w += 1;
                *w
            };
            if warns > self.config.max_warnings_per_call {
                return Verdict::Block(format!(
                    "tool '{name}' has been repeatedly flagged — blocking it now"
                ));
            }
            return Verdict::Warn(format!(
                "you've called '{name}' with the same arguments {count} times; \
                 if it isn't making progress, change approach"
            ));
        }

        Verdict::Allow
    }

    /// Record a tool's result so repeated identical outcomes escalate. Returns
    /// a warning to surface to the model, if any.
    pub fn record_outcome(&mut self, call: &ToolCall, output: &str) -> Option<String> {
        self.record_outcome_parts(&call.name, &call.input, output)
    }

    /// Record by `(name, params, result)` — the OpenFang-shaped entry point.
    pub fn record_outcome_parts(
        &mut self,
        name: &str,
        params: &Value,
        result: &str,
    ) -> Option<String> {
        let prefix = truncate_on_boundary(result, OUTCOME_RESULT_PREFIX);
        let okey = format!("{}||{}", call_key(name, params), prefix);
        let count = {
            let c = self.outcome_counts.entry(okey).or_insert(0);
            *c += 1;
            *c
        };
        if count >= self.config.outcome_block_threshold {
            self.blocked_outcomes.insert(call_key(name, params));
            return Some(format!(
                "tool '{name}' has produced identical results {count} times — \
                 that approach isn't working; further identical calls are blocked"
            ));
        }
        if count >= self.config.outcome_warn_threshold {
            return Some(format!(
                "tool '{name}' returned identical results {count} times; \
                 if it isn't helping, change approach"
            ));
        }
        None
    }

    /// Heuristic: does this look like a polling/status call that legitimately
    /// repeats? Keyword match on the name or arguments.
    pub fn is_poll_call(name: &str, params: &Value) -> bool {
        const KEYWORDS: &[&str] = &["poll", "status", "wait", "watch", "tail"];
        let name_l = name.to_lowercase();
        let args_l = params.to_string().to_lowercase();
        KEYWORDS
            .iter()
            .any(|k| name_l.contains(k) || args_l.contains(k))
    }

    /// A snapshot of guard state.
    pub fn stats(&self) -> LoopGuardStats {
        LoopGuardStats {
            total_calls: self.total_calls,
            distinct_calls: self.call_counts.len(),
            ping_pong_detected: self.ping_pong_detected,
        }
    }

    /// Largest number of full repeating cycles at the tail of the recent-call
    /// window, considering period-2 (A-B-A-B) and period-3 (A-B-C-…) patterns.
    fn max_ping_pong_cycles(&self) -> u32 {
        let items: Vec<&String> = self.recent_calls.iter().collect();
        let mut best = 0;
        for period in [2usize, 3usize] {
            best = best.max(count_pattern_repeats(&items, period));
        }
        best
    }
}

/// A stable key for `(tool name, arguments)`. `serde_json` serializes object
/// keys in sorted order, so semantically-equal args key equal.
fn call_key(name: &str, params: &Value) -> String {
    format!("{name}::{params}")
}

fn truncate_on_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Count how many times the trailing `period`-length pattern repeats
/// consecutively at the end of `items`. Returns 0 if there's no full pattern,
/// or if the pattern is constant (that's per-call repetition, not a cycle).
fn count_pattern_repeats(items: &[&String], period: usize) -> u32 {
    let n = items.len();
    if period == 0 || n < period * 2 {
        return 0;
    }
    let pattern = &items[n - period..];
    if pattern.iter().all(|x| *x == pattern[0]) {
        return 0;
    }
    let mut repeats = 1u32;
    let mut start = n as isize - 2 * period as isize;
    while start >= 0 {
        let s = start as usize;
        if &items[s..s + period] == pattern {
            repeats += 1;
            start -= period as isize;
        } else {
            break;
        }
    }
    repeats
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arg: i64) -> ToolCall {
        ToolCall {
            id: "x".into(),
            name: name.into(),
            input: serde_json::json!({ "n": arg }),
        }
    }

    #[test]
    fn warns_then_blocks_identical_repeats() {
        let mut g = LoopGuard::default(); // warn@3, block@5 (>=)
        assert_eq!(g.check(&call("edit", 1)), Verdict::Allow);
        assert_eq!(g.check(&call("edit", 1)), Verdict::Allow);
        assert!(matches!(g.check(&call("edit", 1)), Verdict::Warn(_))); // 3rd
        assert!(matches!(g.check(&call("edit", 1)), Verdict::Warn(_))); // 4th
        assert!(matches!(g.check(&call("edit", 1)), Verdict::Block(_))); // 5th
    }

    #[test]
    fn poll_tools_get_relaxed_thresholds() {
        let mut g = LoopGuard::default(); // ×3 → warn@9
        for _ in 0..8 {
            assert_eq!(g.check(&call("status_check", 1)), Verdict::Allow);
        }
        assert!(matches!(
            g.check(&call("status_check", 1)),
            Verdict::Warn(_)
        )); // 9th
    }

    #[test]
    fn detects_ping_pong_cycle() {
        let cfg = LoopGuardConfig {
            block_threshold: u32::MAX,
            warn_threshold: u32::MAX,
            ping_pong_min_repeats: 3,
            ..Default::default()
        };
        let mut g = LoopGuard::with_config(cfg);
        for name in ["a", "b", "a", "b", "a"] {
            g.check(&call(name, 1));
        }
        let v = g.check(&call("b", 1)); // completes the 3rd A-B cycle
        assert!(
            matches!(v, Verdict::Block(_)),
            "expected ping-pong block, got {v:?}"
        );
    }

    #[test]
    fn outcome_aware_blocks_identical_results() {
        let cfg = LoopGuardConfig {
            block_threshold: u32::MAX,
            warn_threshold: u32::MAX,
            outcome_block_threshold: 3,
            ..Default::default()
        };
        let mut g = LoopGuard::with_config(cfg);
        let c = call("fetch", 1);
        for _ in 0..3 {
            assert_eq!(g.check(&c), Verdict::Allow);
            g.record_outcome(&c, "same output every time");
        }
        assert!(matches!(g.check(&c), Verdict::Block(_)));
    }

    #[test]
    fn circuit_breaks_on_total_volume() {
        let cfg = LoopGuardConfig {
            warn_threshold: u32::MAX,
            block_threshold: u32::MAX,
            global_circuit_breaker: 3,
            ..Default::default()
        };
        let mut g = LoopGuard::with_config(cfg);
        assert_eq!(g.check(&call("a", 1)), Verdict::Allow);
        assert_eq!(g.check(&call("b", 1)), Verdict::Allow);
        assert_eq!(g.check(&call("c", 1)), Verdict::Allow);
        assert!(matches!(g.check(&call("d", 1)), Verdict::CircuitBreak(_)));
    }
}

/// Battle-tested cases ported from OpenFang's `loop_guard` test suite, adapted
/// to our `(name, params)` entry points (`check_parts`/`record_outcome_parts`).
#[cfg(test)]
mod ported_from_openfang {
    use super::*;

    #[test]
    fn allow_below_threshold() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        let params = serde_json::json!({ "query": "test" });
        assert_eq!(guard.check_parts("web_search", &params), Verdict::Allow);
        assert_eq!(guard.check_parts("web_search", &params), Verdict::Allow);
    }

    #[test]
    fn warn_at_threshold() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        let params = serde_json::json!({ "path": "/etc/passwd" });
        guard.check_parts("file_read", &params); // 1
        guard.check_parts("file_read", &params); // 2
        let v = guard.check_parts("file_read", &params); // 3 = warn
        assert!(matches!(v, Verdict::Warn(_)));
    }

    #[test]
    fn block_at_threshold() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        let params = serde_json::json!({ "command": "ls" });
        for _ in 0..4 {
            guard.check_parts("shell_exec", &params);
        }
        let v = guard.check_parts("shell_exec", &params); // 5 = block
        assert!(matches!(v, Verdict::Block(_)));
    }

    #[test]
    fn different_params_no_collision() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        for i in 0..10 {
            let params = serde_json::json!({ "query": format!("query_{i}") });
            assert_eq!(guard.check_parts("web_search", &params), Verdict::Allow);
        }
    }

    #[test]
    fn global_circuit_breaker() {
        let config = LoopGuardConfig {
            warn_threshold: 100,
            block_threshold: 100,
            global_circuit_breaker: 5,
            ..Default::default()
        };
        let mut guard = LoopGuard::new(config);
        for i in 0..5 {
            let params = serde_json::json!({ "n": i });
            assert_eq!(guard.check_parts("tool", &params), Verdict::Allow);
        }
        let v = guard.check_parts("tool", &serde_json::json!({ "n": 5 }));
        assert!(matches!(v, Verdict::CircuitBreak(_)));
    }

    #[test]
    fn test_outcome_aware_warning() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        let params = serde_json::json!({ "query": "weather" });
        let result = "sunny 72F";
        assert!(
            guard
                .record_outcome_parts("web_search", &params, result)
                .is_none()
        );
        let w = guard.record_outcome_parts("web_search", &params, result);
        assert!(w.is_some());
        assert!(w.unwrap().contains("identical results"));
    }

    #[test]
    fn test_outcome_aware_blocks_next_call() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        let params = serde_json::json!({ "query": "weather" });
        let result = "sunny 72F";
        guard.record_outcome_parts("web_search", &params, result);
        guard.record_outcome_parts("web_search", &params, result);
        let w = guard.record_outcome_parts("web_search", &params, result);
        assert!(w.is_some());
        let v = guard.check_parts("web_search", &params);
        assert!(matches!(v, Verdict::Block(ref msg) if msg.contains("identical results")));
    }

    #[test]
    fn test_ping_pong_ab_detection() {
        let mut guard = LoopGuard::new(LoopGuardConfig {
            warn_threshold: 100,
            block_threshold: 100,
            ping_pong_min_repeats: 3,
            ..Default::default()
        });
        let a = serde_json::json!({ "file": "a.txt" });
        let b = serde_json::json!({ "file": "b.txt" });
        guard.check_parts("file_read", &a);
        guard.check_parts("file_write", &b);
        guard.check_parts("file_read", &a);
        guard.check_parts("file_write", &b);
        guard.check_parts("file_read", &a);
        let v = guard.check_parts("file_write", &b);
        assert!(
            matches!(v, Verdict::Block(ref m) if m.contains("Ping-pong"))
                || matches!(v, Verdict::Warn(ref m) if m.contains("Ping-pong")),
            "expected ping-pong detection, got: {v:?}"
        );
    }

    #[test]
    fn test_ping_pong_abc_detection() {
        let mut guard = LoopGuard::new(LoopGuardConfig {
            warn_threshold: 100,
            block_threshold: 100,
            ping_pong_min_repeats: 3,
            ..Default::default()
        });
        let a = serde_json::json!({ "a": 1 });
        let b = serde_json::json!({ "b": 2 });
        let c = serde_json::json!({ "c": 3 });
        for _ in 0..3 {
            guard.check_parts("tool_a", &a);
            guard.check_parts("tool_b", &b);
            guard.check_parts("tool_c", &c);
        }
        assert!(guard.stats().ping_pong_detected);
    }

    #[test]
    fn test_no_false_ping_pong() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        for i in 0..10 {
            let params = serde_json::json!({ "n": i });
            guard.check_parts("tool", &params);
        }
        assert!(!guard.stats().ping_pong_detected);
    }

    #[test]
    fn test_poll_tool_relaxed_thresholds() {
        let mut guard = LoopGuard::new(LoopGuardConfig::default());
        let params = serde_json::json!({ "command": "docker ps --status running" });
        for _ in 0..8 {
            assert_eq!(
                guard.check_parts("shell_exec", &params),
                Verdict::Allow,
                "poll tool should have relaxed thresholds"
            );
        }
        let v = guard.check_parts("shell_exec", &params);
        assert!(
            matches!(v, Verdict::Warn(_)),
            "expected warn at poll threshold, got: {v:?}"
        );
    }

    #[test]
    fn test_is_poll_call_detection() {
        assert!(LoopGuard::is_poll_call(
            "shell_exec",
            &serde_json::json!({ "command": "docker ps --status" })
        ));
        assert!(LoopGuard::is_poll_call(
            "shell_exec",
            &serde_json::json!({ "command": "tail -f /var/log/app.log" })
        ));
        assert!(!LoopGuard::is_poll_call(
            "shell_exec",
            &serde_json::json!({ "command": "echo hi" })
        ));
        assert!(!LoopGuard::is_poll_call(
            "file_read",
            &serde_json::json!({ "path": "/tmp/x" })
        ));
    }
}
