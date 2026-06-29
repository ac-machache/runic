//! Folding the server's event stream (live SSE wire events + persisted
//! `{seq,event}` log entries) into the UI model: the chat `items` list and the
//! Events-tab Run→Turn clusters.

use leptos::prelude::*;
use serde_json::Value;

use crate::model::{Item, LiveBuf, LiveKind, PendingAsk, RunCluster, ToolView, TurnCluster};
use crate::util::{content_blocks, first_text, parse_sources, pretty_json, truncate};

// ── Events tab: cluster a flat event list into Run → Turn → details ───────

/// Cluster a flat event list (live wire events OR persisted `{seq,event}`
/// entries) into Run → Turn → details. Coalesces token deltas; never one row
/// per token.
pub fn cluster_runs(events: &[Value]) -> Vec<RunCluster> {
    let mut runs: Vec<RunCluster> = Vec::new();
    for entry in events {
        let (disc, ev): (String, &Value) = match entry.get("event") {
            Some(inner) => (
                inner
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                inner,
            ),
            None => (
                entry
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                entry,
            ),
        };
        match disc.as_str() {
            // UI-injected boundary for live runs (carries the user prompt).
            "run_begin" => {
                let prompt = ev
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                runs.push(RunCluster {
                    prompt,
                    running: true,
                    ..Default::default()
                });
            }
            "run_start" | "RunStart" => {
                let id = ev
                    .get("run_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // A live run opens with a UI-injected `run_begin` (carrying the
                // prompt); the wire `run_start` that immediately follows is the
                // SAME run — adopt its id rather than starting a second, empty
                // cluster. For replay (no `run_begin`) the last run is already
                // ended, so a fresh cluster is created.
                match runs.last_mut() {
                    Some(r) if r.running && r.id.is_empty() && r.turns.is_empty() => r.id = id,
                    _ => runs.push(RunCluster {
                        id,
                        running: true,
                        ..Default::default()
                    }),
                }
            }
            "assistant_text_delta" => {
                let t = ev.get("text").and_then(|v| v.as_str()).unwrap_or("");
                cur_turn(&mut runs).text.push_str(t);
            }
            "assistant_thinking_delta" => {
                let t = ev.get("text").and_then(|v| v.as_str()).unwrap_or("");
                cur_turn(&mut runs).thinking.push_str(t);
            }
            "tool_start" => {
                let id = ev
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = ev
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let input = ev.get("input").map(pretty_json).unwrap_or_default();
                cur_turn(&mut runs).tools.push(ToolView {
                    id,
                    name,
                    input,
                    status: "running".into(),
                    ..Default::default()
                });
            }
            // `tool_dispatching` is no longer emitted live, but persisted-replay
            // Message events still carry tool input; this arm stays harmless.
            "tool_dispatching" => {
                let id = ev
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = ev.get("input").map(pretty_json).unwrap_or_default();
                if let Some(t) = find_tool(&mut runs, &id) {
                    t.input = input;
                }
            }
            // Live `tool_finish` now carries only {id,name,is_error}; preview /
            // duration / metadata default away (they arrive via persisted replay).
            "tool_finish" => {
                let id = ev
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_err = ev
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let preview = ev
                    .get("preview")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let dur = ev.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                let sources = parse_sources(ev.get("metadata"));
                if let Some(t) = find_tool(&mut runs, &id) {
                    t.status = if is_err {
                        "error".into()
                    } else {
                        "done".into()
                    };
                    t.result = preview;
                    t.duration_ms = dur;
                    t.sources = sources;
                }
            }
            "Message" => ingest_persisted(&mut runs, ev.get("msg")),
            "turn_complete" | "TurnBoundary" => {
                if let Some(run) = runs.last_mut()
                    && let Some(turn) = run.turns.last_mut()
                {
                    turn.closed = true;
                    if let Some(sr) = ev.get("stop_reason").and_then(|v| v.as_str()) {
                        turn.stop_reason = Some(sr.to_string());
                    }
                    match ev.get("tool_calls_this_turn").and_then(|v| v.as_u64()) {
                        Some(tc) => turn.tool_calls = tc as u32,
                        None if turn.tool_calls == 0 => turn.tool_calls = turn.tools.len() as u32,
                        None => {}
                    }
                }
            }
            "run_end" | "RunEnd" | "done" => {
                if let Some(run) = runs.last_mut() {
                    run.running = false;
                    run.ended = true;
                    if let Some(sr) = ev.get("stop_reason").and_then(|v| v.as_str()) {
                        run.stop_reason = Some(sr.to_string());
                    }
                    if let Some(t) = run.turns.last_mut() {
                        t.closed = true;
                        if t.tool_calls == 0 {
                            t.tool_calls = t.tools.len() as u32;
                        }
                    }
                }
            }
            "usage" => {
                if let Some(run) = runs.last_mut() {
                    let i = ev.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let o = ev
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    run.usage = Some((i, o));
                }
            }
            "warning" => {
                if let Some(run) = runs.last_mut() {
                    run.errored = true;
                }
            }
            "run_error" => {
                if let Some(run) = runs.last_mut() {
                    run.running = false;
                    run.ended = true;
                    run.errored = true;
                    if let Some(m) = ev.get("message").and_then(|v| v.as_str()) {
                        run.stop_reason = Some(m.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    runs
}

/// Current open turn of the current run, creating a run/turn as needed.
fn cur_turn(runs: &mut Vec<RunCluster>) -> &mut TurnCluster {
    let need_new_run = match runs.last() {
        Some(r) => r.ended,
        None => true,
    };
    if need_new_run {
        runs.push(RunCluster {
            running: true,
            ..Default::default()
        });
    }
    let run = runs.last_mut().unwrap();
    let need_new = match run.turns.last() {
        Some(t) => t.closed,
        None => true,
    };
    if need_new {
        run.turns.push(TurnCluster::default());
    }
    run.turns.last_mut().unwrap()
}

/// Find a tool (by id) in the current run, searching newest-first.
fn find_tool<'a>(runs: &'a mut [RunCluster], id: &str) -> Option<&'a mut ToolView> {
    let run = runs.last_mut()?;
    for turn in run.turns.iter_mut().rev() {
        if let Some(t) = turn.tools.iter_mut().rev().find(|t| t.id == id) {
            return Some(t);
        }
    }
    None
}

/// Fold a persisted `Message` into the run/turn clusters.
fn ingest_persisted(runs: &mut Vec<RunCluster>, msg: Option<&Value>) {
    let Some(msg) = msg else { return };
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let blocks = content_blocks(msg);
    if role == "assistant" {
        let turn = cur_turn(runs);
        for b in &blocks {
            match b.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        turn.text.push_str(t);
                    }
                }
                Some("reasoning") => {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        turn.thinking.push_str(t);
                    }
                }
                Some("tool_use") => {
                    turn.tools.push(ToolView {
                        id: b
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: b
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("tool")
                            .to_string(),
                        input: b.get("input").map(pretty_json).unwrap_or_default(),
                        status: "done".into(),
                        ..Default::default()
                    });
                }
                _ => {}
            }
        }
        turn.tool_calls = turn.tools.len() as u32;
    } else if role == "user" {
        // Plain user text is the run's prompt; tool_result blocks attach to
        // the tool they answer.
        if let Some(run) = runs.last_mut()
            && run.prompt.is_empty()
        {
            for b in &blocks {
                if b.get("type").and_then(|v| v.as_str()) == Some("text")
                    && let Some(t) = b.get("text").and_then(|v| v.as_str())
                {
                    run.prompt = t.to_string();
                }
            }
        }
        for b in &blocks {
            if b.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                let id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                let is_err = b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                let content = b
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let sources = parse_sources(b.get("metadata"));
                if let Some(t) = find_tool(runs, id) {
                    t.status = if is_err {
                        "error".into()
                    } else {
                        "done".into()
                    };
                    t.result = truncate(&content, 400);
                    t.sources = sources;
                }
            }
        }
    }
}

// ── live → items folding (chat pane) ─────────────────────────────────────

pub fn append_live(
    live: RwSignal<LiveBuf>,
    items: RwSignal<Vec<Item>>,
    kind: LiveKind,
    text: &str,
) {
    live.update(|lb| {
        if lb.kind != kind && !lb.text.is_empty() {
            items.update(|its| its.push(finalize(lb.kind, &lb.text)));
            lb.text.clear();
        }
        lb.kind = kind;
        lb.text.push_str(text);
    });
}

pub fn flush_live(live: RwSignal<LiveBuf>, items: RwSignal<Vec<Item>>) {
    live.update(|lb| {
        if !lb.text.is_empty() {
            items.update(|its| its.push(finalize(lb.kind, &lb.text)));
        }
        lb.kind = LiveKind::None;
        lb.text.clear();
    });
}

fn finalize(kind: LiveKind, text: &str) -> Item {
    match kind {
        LiveKind::Thinking => Item::Thinking(text.to_string()),
        _ => Item::Assistant(text.to_string()),
    }
}

/// Rebuild the chat transcript from a persisted event log (history load).
pub fn items_from_events(events: &[Value]) -> Vec<Item> {
    let mut items = Vec::new();
    for entry in events {
        let event = entry.get("event").unwrap_or(entry);
        match event.get("kind").and_then(|v| v.as_str()) {
            Some("Message") => {
                if let Some(msg) = event.get("msg") {
                    ingest_message(&mut items, msg);
                }
            }
            Some("StateSnapshot") => {
                if let Some(msgs) = event.get("messages").and_then(|v| v.as_array()) {
                    items.clear();
                    for m in msgs {
                        ingest_message(&mut items, m);
                    }
                }
            }
            _ => {}
        }
    }
    items
}

fn ingest_message(items: &mut Vec<Item>, msg: &Value) {
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let blocks = content_blocks(msg);
    for b in &blocks {
        match b.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                let t = b
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if t.is_empty() {
                    continue;
                }
                if role == "user" {
                    items.push(Item::User(t));
                } else {
                    items.push(Item::Assistant(t));
                }
            }
            Some("tool_use") => {
                items.push(Item::Tool(ToolView {
                    id: b
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    name: b
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool")
                        .to_string(),
                    input: b
                        .get("input")
                        .map(|v| truncate(&v.to_string(), 300))
                        .unwrap_or_default(),
                    status: "done".to_string(),
                    ..Default::default()
                }));
            }
            Some("tool_result") => {
                let id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                let is_error = b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                let content = b
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let sources = parse_sources(b.get("metadata"));
                if let Some(Item::Tool(t)) = items
                    .iter_mut()
                    .rev()
                    .find(|i| matches!(i, Item::Tool(t) if t.id == id))
                {
                    t.status = if is_error {
                        "error".into()
                    } else {
                        "done".into()
                    };
                    t.result = truncate(&content, 600);
                    t.sources = sources;
                }
            }
            _ => {}
        }
    }
}

/// Apply one live wire event to the chat `items` list.
pub fn apply_event(items: &mut Vec<Item>, ev: &Value) {
    let kind = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "assistant_text_delta" => {
            let text = ev.get("text").and_then(|v| v.as_str()).unwrap_or("");
            match items.last_mut() {
                Some(Item::Assistant(s)) => s.push_str(text),
                _ => items.push(Item::Assistant(text.to_string())),
            }
        }
        "assistant_thinking_delta" => {
            let text = ev.get("text").and_then(|v| v.as_str()).unwrap_or("");
            match items.last_mut() {
                Some(Item::Thinking(s)) => s.push_str(text),
                _ => items.push(Item::Thinking(text.to_string())),
            }
        }
        "tool_start" => {
            items.push(Item::Tool(ToolView {
                id: ev
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                name: ev
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string(),
                input: ev.get("input").map(pretty_json).unwrap_or_default(),
                status: "running".to_string(),
                ..Default::default()
            }));
        }
        // No longer emitted live; harmless if it ever arrives.
        "tool_dispatching" => {
            let id = ev.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(Item::Tool(t)) = items
                .iter_mut()
                .rev()
                .find(|i| matches!(i, Item::Tool(t) if t.id == id))
            {
                t.input = ev
                    .get("input")
                    .map(|v| truncate(&v.to_string(), 300))
                    .unwrap_or_default();
            }
        }
        "tool_finish" => {
            let id = ev.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let is_error = ev
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let preview = ev
                .get("preview")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let dur = ev.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
            let sources = parse_sources(ev.get("metadata"));
            if let Some(Item::Tool(t)) = items
                .iter_mut()
                .rev()
                .find(|i| matches!(i, Item::Tool(t) if t.id == id))
            {
                t.status = if is_error {
                    "error".into()
                } else {
                    "done".into()
                };
                t.result = preview;
                t.duration_ms = dur;
                t.sources = sources;
            }
        }
        "warning" => {
            let m = ev
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            items.push(Item::Warning(m));
        }
        // HITL fire-and-forget: surface it as a note in the transcript.
        "escalated" => {
            let reason = ev.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            items.push(Item::Warning(format!("escalated to human: {reason}")));
        }
        "run_error" => {
            let m = ev
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("run failed");
            items.push(Item::Warning(format!("run failed: {m}")));
        }
        "message" => {
            if let Some(msg) = ev.get("msg") {
                let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if role == "user" {
                    if let Some(text) = first_text(msg)
                        && !matches!(items.last(), Some(Item::User(u)) if *u == text)
                    {
                        items.push(Item::User(text));
                    }
                } else if role == "assistant"
                    && let Some(text) = first_text(msg)
                    && !matches!(items.last(), Some(Item::Assistant(_)))
                {
                    items.push(Item::Assistant(text));
                }
            }
        }
        _ => {}
    }
}

/// Extract a `usage` event's `(input, output)` token counts.
pub fn usage_of(ev: &Value) -> Option<(&'static str, (u64, u64))> {
    if ev.get("type").and_then(|v| v.as_str()) == Some("usage") {
        let i = ev.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let o = ev
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Some(("usage", (i, o)))
    } else {
        None
    }
}

/// Parse an `ask_required` event (the new `HumanInterface` HITL prompt).
pub fn parse_ask(ev: &Value) -> Option<PendingAsk> {
    let ask_id = ev.get("ask_id")?.as_str()?.to_string();
    let question = ev
        .get("question")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let context = ev
        .get("context")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Some(PendingAsk {
        ask_id,
        question,
        context,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, msg: Value) -> Value {
        serde_json::json!({ "seq": 1, "event": { "kind": kind, "msg": msg } })
    }

    #[test]
    fn reconstructs_history_with_lowercase_roles() {
        let events = vec![
            ev("RunStart", Value::Null),
            ev(
                "Message",
                serde_json::json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] }),
            ),
            ev(
                "Message",
                serde_json::json!({ "role": "assistant", "content": [{ "type": "text", "text": "hello!" }] }),
            ),
        ];
        let items = items_from_events(&events);
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], Item::User(t) if t == "hi"));
        assert!(matches!(&items[1], Item::Assistant(t) if t == "hello!"));
    }

    #[test]
    fn pairs_tool_use_with_its_result() {
        let events = vec![
            ev(
                "Message",
                serde_json::json!({ "role": "assistant", "content": [
                { "type": "tool_use", "id": "call_1", "name": "echo", "input": { "msg": "x" } }
            ] }),
            ),
            ev(
                "Message",
                serde_json::json!({ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "call_1", "content": "ran echo", "is_error": false }
            ] }),
            ),
        ];
        let items = items_from_events(&events);
        assert_eq!(items.len(), 1);
        match &items[0] {
            Item::Tool(t) => {
                assert_eq!(t.name, "echo");
                assert_eq!(t.status, "done");
                assert!(t.result.contains("ran echo"));
            }
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    #[test]
    fn clusters_live_run_into_turns() {
        let events = vec![
            serde_json::json!({ "type": "run_start", "run_id": "r1" }),
            serde_json::json!({ "type": "assistant_text_delta", "text": "Look" }),
            serde_json::json!({ "type": "assistant_text_delta", "text": "ing." }),
            serde_json::json!({ "type": "tool_start", "id": "c1", "name": "mcp__tavily__search" }),
            serde_json::json!({ "type": "tool_finish", "id": "c1", "is_error": false }),
            serde_json::json!({ "type": "turn_complete", "turn": 1, "stop_reason": "tool_use" }),
            serde_json::json!({ "type": "assistant_text_delta", "text": "Done" }),
            serde_json::json!({ "type": "run_end", "run_id": "r1", "total_turns": 2, "stop_reason": "end_turn" }),
            serde_json::json!({ "type": "usage", "input_tokens": 100, "output_tokens": 20 }),
        ];
        let runs = cluster_runs(&events);
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.id, "r1");
        assert!(!r.running);
        assert_eq!(r.usage, Some((100, 20)));
        assert_eq!(r.turns.len(), 2);
        assert_eq!(r.turns[0].text, "Looking.");
        assert_eq!(r.turns[0].tools.len(), 1);
        assert_eq!(r.turns[0].tools[0].status, "done");
        assert_eq!(r.turns[0].stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(r.turns[1].text, "Done");
    }

    #[test]
    fn run_begin_separates_live_runs_and_keeps_prompt() {
        let events = vec![
            serde_json::json!({ "type": "run_begin", "prompt": "first question" }),
            serde_json::json!({ "type": "assistant_text_delta", "text": "answer one" }),
            serde_json::json!({ "type": "done", "total_turns": 1 }),
            serde_json::json!({ "type": "run_begin", "prompt": "second question" }),
            serde_json::json!({ "type": "assistant_text_delta", "text": "answer two" }),
            serde_json::json!({ "type": "done", "total_turns": 1 }),
        ];
        let runs = cluster_runs(&events);
        assert_eq!(runs.len(), 2, "each run_begin..done is a distinct run");
        assert_eq!(runs[0].prompt, "first question");
        assert_eq!(runs[0].turns[0].text, "answer one");
        assert!(runs[0].ended);
        assert_eq!(runs[1].prompt, "second question");
        assert_eq!(runs[1].turns[0].text, "answer two");
    }

    #[test]
    fn state_snapshot_replaces_history() {
        let events = vec![
            ev(
                "Message",
                serde_json::json!({ "role": "user", "content": [{ "type": "text", "text": "old" }] }),
            ),
            serde_json::json!({ "seq": 2, "event": { "kind": "StateSnapshot", "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "summary" }] }
            ] } }),
        ];
        let items = items_from_events(&events);
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], Item::User(t) if t == "summary"));
    }

    #[test]
    fn handles_string_content_messages() {
        // The new MessageContent serializes simple text as a bare string.
        let events = vec![
            ev(
                "Message",
                serde_json::json!({ "role": "user", "content": "hello there" }),
            ),
            ev(
                "Message",
                serde_json::json!({ "role": "assistant", "content": "hi!" }),
            ),
        ];
        let items = items_from_events(&events);
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], Item::User(t) if t == "hello there"));
        assert!(matches!(&items[1], Item::Assistant(t) if t == "hi!"));
    }

    #[test]
    fn run_prompt_comes_from_string_content_user_message() {
        let events = vec![
            serde_json::json!({ "seq": 1, "event": { "kind": "RunStart", "run_id": "r1" } }),
            ev(
                "Message",
                serde_json::json!({ "role": "user", "content": "what time is it?" }),
            ),
        ];
        let runs = cluster_runs(&events);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].prompt, "what time is it?");
    }

    #[test]
    fn run_begin_and_wire_run_start_are_one_run() {
        // The live flow: UI injects run_begin (prompt), then the wire sends
        // run_start. They must collapse into a SINGLE run, not two.
        let events = vec![
            serde_json::json!({ "type": "run_begin", "prompt": "what time is it?" }),
            serde_json::json!({ "type": "run_start", "run_id": "r-42" }),
            serde_json::json!({ "type": "tool_start", "id": "c1", "name": "system_time" }),
            serde_json::json!({ "type": "tool_finish", "id": "c1", "is_error": false }),
            serde_json::json!({ "type": "assistant_text_delta", "text": "It is noon." }),
            serde_json::json!({ "type": "done", "total_turns": 2 }),
        ];
        let runs = cluster_runs(&events);
        assert_eq!(runs.len(), 1, "run_begin + run_start must be one run");
        assert_eq!(runs[0].prompt, "what time is it?");
        assert_eq!(runs[0].id, "r-42");
        assert!(runs[0].ended);
    }

    #[test]
    fn parse_ask_reads_the_new_hitl_event() {
        let ev = serde_json::json!({ "type": "ask_required", "ask_id": "a1", "question": "proceed?", "context": "ctx" });
        let p = parse_ask(&ev).expect("parsed");
        assert_eq!(p.ask_id, "a1");
        assert_eq!(p.question, "proceed?");
        assert_eq!(p.context.as_deref(), Some("ctx"));
    }
}
