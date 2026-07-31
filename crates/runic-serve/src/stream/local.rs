use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use super::sink::{Replay, RunEvents};
use crate::wire::WireEvent;

const MAX_BYTES: usize = 2 * 1024 * 1024;
const TRACKED_RUNS: usize = 1024;

fn weight(event: &WireEvent) -> usize {
    const OVERHEAD: usize = 64;
    let carried = match event {
        WireEvent::AssistantTextDelta { text } => text.len(),
        WireEvent::AssistantThinkingDelta { text } => text.len(),
        WireEvent::ToolStart { name, input, .. } => name.len() + input.to_string().len(),
        WireEvent::ToolFinish { name, preview, .. } => name.len() + preview.len(),
        _ => 0,
    };
    OVERHEAD + carried
}

#[derive(Default)]
struct Buffer {
    events: VecDeque<(u64, WireEvent)>,
    bytes: usize,
    next_seq: u64,
    closed: bool,
}

impl Buffer {
    fn push(&mut self, event: WireEvent) {
        self.next_seq += 1;
        self.bytes += weight(&event);
        self.events.push_back((self.next_seq, event));
        while self.bytes > MAX_BYTES {
            match self.events.pop_front() {
                Some((_, dropped)) => self.bytes = self.bytes.saturating_sub(weight(&dropped)),
                None => break,
            }
        }
    }

    fn since(&self, after: u64) -> Replay {
        let oldest = self.events.front().map(|(seq, _)| *seq);
        let gap = oldest.is_some_and(|first| after + 1 < first);
        Replay {
            events: self
                .events
                .iter()
                .filter(|(seq, _)| *seq > after)
                .cloned()
                .collect(),
            gap,
            closed: self.closed,
        }
    }
}

#[derive(Default)]
struct Run {
    buffer: Mutex<Buffer>,
    arrived: Notify,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> WireEvent {
        WireEvent::AssistantTextDelta {
            text: text.to_string(),
        }
    }

    const CHUNK: usize = 1024;

    fn filled(count: usize) -> Buffer {
        let mut buffer = Buffer::default();
        for index in 0..count {
            buffer.push(delta(&index.to_string()));
        }
        buffer
    }

    fn overfilled() -> Buffer {
        let chunk = "x".repeat(CHUNK);
        let mut buffer = Buffer::default();
        for _ in 0..(MAX_BYTES / CHUNK + 8) {
            buffer.push(delta(&chunk));
        }
        buffer
    }

    #[test]
    fn a_reader_inside_the_window_sees_no_gap() {
        let buffer = filled(10);
        let replay = buffer.since(3);
        assert!(!replay.gap);
        assert_eq!(replay.events.len(), 7);
        assert_eq!(
            replay.events[0].0, 4,
            "resumes at the event after the cursor"
        );
    }

    #[test]
    fn a_reader_left_behind_by_the_window_gets_a_gap() {
        let buffer = overfilled();
        let replay = buffer.since(3);
        assert!(
            replay.gap,
            "cursor 3 fell out of the window, the reader must be told"
        );
    }

    #[test]
    fn the_oldest_surviving_cursor_is_still_gapless() {
        let buffer = overfilled();
        let oldest = buffer.events.front().expect("window is full").0;
        let replay = buffer.since(oldest - 1);
        assert!(
            !replay.gap,
            "the reader asked for exactly what is still held"
        );
        assert_eq!(replay.events.len(), buffer.events.len());
        assert!(buffer.bytes <= MAX_BYTES);
    }

    #[test]
    fn a_stream_of_tiny_events_is_bounded_by_bytes_alone() {
        let buffer = filled(200_000);
        assert!(
            buffer.bytes <= MAX_BYTES,
            "held {} bytes over {} events",
            buffer.bytes,
            buffer.events.len()
        );
        assert!(
            buffer.events.len() > 10_000,
            "small events should be kept in quantity, not capped by an arbitrary count"
        );
    }

    #[test]
    fn a_few_huge_chunks_evict_as_hard_as_many_small_ones() {
        const HUGE: usize = 64 * 1024;
        let pushed = MAX_BYTES / HUGE + 4;

        let mut buffer = Buffer::default();
        let chunk = "x".repeat(HUGE);
        for _ in 0..pushed {
            buffer.push(delta(&chunk));
        }
        assert!(
            buffer.bytes <= MAX_BYTES,
            "byte budget held at {} bytes",
            buffer.bytes
        );
        assert!(
            buffer.events.len() < pushed,
            "huge chunks must evict, however few of them there are"
        );
    }

    #[test]
    fn a_finished_run_reports_closed_with_nothing_left() {
        let mut buffer = filled(20);
        buffer.events.clear();
        buffer.closed = true;
        let replay = buffer.since(3);
        assert!(replay.closed);
        assert!(!replay.gap, "an emptied buffer is not a gap, it is the end");
        assert!(replay.events.is_empty());
    }

    #[test]
    fn eviction_prefers_a_finished_run() {
        let mut live = Live::default();
        let keep = live.get_or_open("still-running");
        let done = live.get_or_open("finished");
        done.buffer.lock().expect("buffer").closed = true;

        live.evict_oldest();

        assert!(live.runs.contains_key("still-running"));
        assert!(!live.runs.contains_key("finished"));
        drop(keep);
    }
}

#[derive(Default)]
struct Live {
    runs: HashMap<String, Arc<Run>>,
    order: VecDeque<String>,
}

impl Live {
    fn get_or_open(&mut self, run_id: &str) -> Arc<Run> {
        if let Some(run) = self.runs.get(run_id) {
            return Arc::clone(run);
        }
        while self.order.len() >= TRACKED_RUNS {
            self.evict_oldest();
        }
        let run = Arc::new(Run::default());
        self.runs.insert(run_id.to_string(), Arc::clone(&run));
        self.order.push_back(run_id.to_string());
        run
    }

    fn evict_oldest(&mut self) {
        let finished = self.order.iter().position(|run_id| {
            self.runs
                .get(run_id)
                .is_some_and(|run| run.buffer.lock().expect("stream buffer poisoned").closed)
        });
        let victim = match finished {
            Some(index) => self.order.remove(index),
            None => self.order.pop_front(),
        };
        if let Some(run_id) = victim {
            self.runs.remove(&run_id);
        }
    }
}

#[derive(Default)]
pub struct LocalEvents {
    live: Mutex<Live>,
}

impl LocalEvents {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn run(&self, run_id: &str) -> Arc<Run> {
        self.live
            .lock()
            .expect("stream buffers poisoned")
            .get_or_open(run_id)
    }
}

#[async_trait::async_trait]
impl RunEvents for LocalEvents {
    fn publish(&self, run_id: &str, event: WireEvent) {
        let run = self.run(run_id);
        run.buffer
            .lock()
            .expect("stream buffer poisoned")
            .push(event);
        run.arrived.notify_waiters();
    }

    fn finish(&self, run_id: &str) {
        let run = self.run(run_id);
        {
            let mut buffer = run.buffer.lock().expect("stream buffer poisoned");
            buffer.events.clear();
            buffer.bytes = 0;
            buffer.closed = true;
        }
        run.arrived.notify_waiters();
    }

    async fn since(&self, run_id: &str, after: u64) -> Replay {
        let run = self.run(run_id);
        loop {
            let waiting = run.arrived.notified();
            let replay = run
                .buffer
                .lock()
                .expect("stream buffer poisoned")
                .since(after);
            if !replay.events.is_empty() || replay.closed || replay.gap {
                return replay;
            }
            waiting.await;
        }
    }
}
