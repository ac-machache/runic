//! `SessionStore` contract — the guarantees every backend must uphold.
//!
//! Cases take `&dyn SessionStore` and mint their own unique ids. Events have no
//! `PartialEq`, so "exact roundtrip" is asserted by comparing `serde_json::Value`
//! (which is also precisely the on-the-wire payload the store must preserve).

use chrono::{DateTime, Utc};
use serde_json::Value;

use runic_state::{HookLifecycle, RunOutcome, SessionEvent};
use runic_substrate::{RunStatus, SessionStore, StoredEvent, replay_into_state, replay_messages};
use runic_types::{ContentBlock, Message, TokenUsage};

use crate::common::ids::{tenant_session, uid};

// ── builders ──────────────────────────────────────────────────────────────────

/// Microsecond-precision timestamp; nanos are dropped so the value survives a
/// Postgres `TIMESTAMPTZ` (µs) roundtrip identically to the in-RAM backends.
fn ts(offset_micros: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(1_700_000_000_000_000 + offset_micros).unwrap()
}

fn run_start(run: &str, n: i64) -> SessionEvent {
    SessionEvent::RunStart {
        run_id: run.into(),
        agent: None,
        at: ts(n),
    }
}

fn run_end(run: &str, stop: Option<&str>, n: i64) -> SessionEvent {
    SessionEvent::RunEnd {
        run_id: run.into(),
        outcome: RunOutcome {
            total_turns: 3,
            stop_reason: stop.map(str::to_string),
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 42,
            },
            structured: None,
        },
        at: ts(n),
    }
}

fn user_msg(run: &str, text: &str, n: i64) -> SessionEvent {
    SessionEvent::Message {
        run_id: run.into(),
        msg: Message::user(text),
        at: ts(n),
    }
}

fn assistant_msg(run: &str, text: &str, n: i64) -> SessionEvent {
    SessionEvent::Message {
        run_id: run.into(),
        msg: Message::assistant(text),
        at: ts(n),
    }
}

fn value(e: &SessionEvent) -> Value {
    serde_json::to_value(e).expect("event serializes")
}

fn text_of(e: &SessionEvent) -> Option<String> {
    match e {
        SessionEvent::Message { msg, .. } => Some(msg.content.text_content()),
        _ => None,
    }
}

/// Walk the whole log via the paginating reader, `page` events at a time.
async fn paginate(
    store: &dyn SessionStore,
    tenant: &str,
    sid: &str,
    page: usize,
) -> Vec<StoredEvent> {
    let mut out = Vec::new();
    let mut after = 0u64;
    loop {
        let batch = store
            .read_after_limited(tenant, sid, after, page)
            .await
            .unwrap();
        if batch.is_empty() {
            break;
        }
        after = batch.last().unwrap().seq;
        out.extend(batch);
    }
    out
}

// ── core event log ──────────────────────────────────────────────────────────

pub async fn empty_read_returns_empty(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    assert!(store.read(&t, &s).await.unwrap().is_empty());
}

pub async fn append_one_then_read_one(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let seq = store
        .append(&t, &s, &user_msg("r", "hello", 0))
        .await
        .unwrap();
    assert_eq!(seq, 1);
    let read = store.read(&t, &s).await.unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].seq, 1);
    assert_eq!(text_of(&read[0].event).as_deref(), Some("hello"));
}

pub async fn append_many_preserves_insertion_order(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let n = 25;
    for i in 0..n {
        store
            .append(&t, &s, &user_msg("r", &format!("m{i}"), i))
            .await
            .unwrap();
    }
    let read = store.read(&t, &s).await.unwrap();
    assert_eq!(read.len(), n as usize);
    let got: Vec<String> = read.iter().filter_map(|e| text_of(&e.event)).collect();
    let want: Vec<String> = (0..n).map(|i| format!("m{i}")).collect();
    assert_eq!(got, want);
}

pub async fn append_batch_preserves_order(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let batch = vec![
        user_msg("r", "a", 0),
        assistant_msg("r", "b", 1),
        user_msg("r", "c", 2),
    ];
    store.append_batch(&t, &s, &batch).await.unwrap();
    let read = store.read(&t, &s).await.unwrap();
    let got: Vec<String> = read.iter().filter_map(|e| text_of(&e.event)).collect();
    assert_eq!(got, vec!["a", "b", "c"]);
}

pub async fn multiple_batches_preserve_global_order(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .append_batch(&t, &s, &[user_msg("r", "a0", 0), user_msg("r", "a1", 1)])
        .await
        .unwrap();
    store
        .append_batch(&t, &s, &[user_msg("r", "b0", 2), user_msg("r", "b1", 3)])
        .await
        .unwrap();
    let read = store.read(&t, &s).await.unwrap();
    let got: Vec<String> = read.iter().filter_map(|e| text_of(&e.event)).collect();
    assert_eq!(got, vec!["a0", "a1", "b0", "b1"]);
}

pub async fn seqs_are_monotonic_and_gapless(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    for i in 0..30 {
        store
            .append(&t, &s, &user_msg("r", &format!("m{i}"), i))
            .await
            .unwrap();
    }
    let seqs: Vec<u64> = store
        .read(&t, &s)
        .await
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(seqs, (1..=30).collect::<Vec<_>>());
}

pub async fn read_after_zero_returns_all(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    for i in 0..10 {
        store
            .append(&t, &s, &user_msg("r", &format!("m{i}"), i))
            .await
            .unwrap();
    }
    assert_eq!(store.read_after(&t, &s, 0).await.unwrap().len(), 10);
}

pub async fn read_after_last_seq_returns_empty(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    for i in 0..10 {
        store
            .append(&t, &s, &user_msg("r", &format!("m{i}"), i))
            .await
            .unwrap();
    }
    let last = store.read(&t, &s).await.unwrap().last().unwrap().seq;
    assert!(store.read_after(&t, &s, last).await.unwrap().is_empty());
}

/// Paginating the full log (incl. run/turn-boundary events) yields every event
/// exactly once, in order — no skips, no duplicates, cursor never stuck.
pub async fn pagination_covers_every_event_once(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    // A mixed log so the cursor crosses non-Message events too.
    let mut events = vec![run_start("r1", 0)];
    for i in 0..20 {
        events.push(user_msg("r1", &format!("m{i}"), 1 + i));
        if i % 5 == 4 {
            events.push(SessionEvent::TurnBoundary {
                run_id: "r1".into(),
                at: ts(100 + i),
            });
        }
    }
    events.push(run_end("r1", Some("end_turn"), 200));
    let total = events.len();
    store.append_batch(&t, &s, &events).await.unwrap();

    for page in [1usize, 3, 7, 1000] {
        let walked = paginate(store, &t, &s, page).await;
        let seqs: Vec<u64> = walked.iter().map(|e| e.seq).collect();
        assert_eq!(seqs.len(), total, "page={page}: wrong count");
        assert_eq!(
            seqs,
            (1..=total as u64).collect::<Vec<_>>(),
            "page={page}: skip/dup/order"
        );
    }
}

pub async fn read_run_after_filters_by_run(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .append_batch(
            &t,
            &s,
            &[
                run_start("r1", 0),
                user_msg("r1", "one", 1),
                run_end("r1", Some("end_turn"), 2),
                run_start("r2", 3),
                user_msg("r2", "two", 4),
            ],
        )
        .await
        .unwrap();
    let r2 = store.read_run_after(&t, &s, "r2", 0).await.unwrap();
    assert!(r2.iter().all(|e| e.event.run_id() == "r2"));
    assert_eq!(r2.len(), 2);
}

// ── isolation ─────────────────────────────────────────────────────────────────

pub async fn append_isolated_across_sessions(store: &dyn SessionStore) {
    let t = uid("tenant");
    let (s1, s2) = (uid("sess"), uid("sess"));
    store.append(&t, &s1, &user_msg("r", "x", 0)).await.unwrap();
    assert_eq!(store.read(&t, &s1).await.unwrap().len(), 1);
    assert!(store.read(&t, &s2).await.unwrap().is_empty());
}

pub async fn append_isolated_across_tenants(store: &dyn SessionStore) {
    let (t1, t2) = (uid("tenant"), uid("tenant"));
    let s = uid("sess");
    store.append(&t1, &s, &user_msg("r", "x", 0)).await.unwrap();
    assert_eq!(store.read(&t1, &s).await.unwrap().len(), 1);
    assert!(store.read(&t2, &s).await.unwrap().is_empty());
}

pub async fn same_session_id_different_tenants_isolated(store: &dyn SessionStore) {
    let (t1, t2) = (uid("tenant"), uid("tenant"));
    let s = uid("sess"); // SAME id under both tenants
    store
        .append(&t1, &s, &user_msg("r", "alice", 0))
        .await
        .unwrap();
    store
        .append(&t2, &s, &user_msg("r", "bob1", 0))
        .await
        .unwrap();
    store
        .append(&t2, &s, &user_msg("r", "bob2", 1))
        .await
        .unwrap();
    assert_eq!(store.read(&t1, &s).await.unwrap().len(), 1);
    assert_eq!(store.read(&t2, &s).await.unwrap().len(), 2);
    assert_eq!(
        text_of(&store.read(&t1, &s).await.unwrap()[0].event).as_deref(),
        Some("alice")
    );
}

/// Tenant/session ids with slashes, underscores, spaces, unicode, percent signs
/// never collide or leak — even the deliberately-confusable `a/b` vs `a_b`.
pub async fn weird_tenant_and_session_ids_isolated(store: &dyn SessionStore) {
    let base = uid("x");
    let pairs = [
        (format!("{base}/a"), format!("{base}/s")),
        (format!("{base}_a"), format!("{base}_s")),
        (format!("{base} a"), format!("{base} s")),
        (format!("{base}%2e"), format!("{base}%2f")),
        (format!("{base}-café-π"), format!("{base}-世界")),
    ];
    for (i, (t, s)) in pairs.iter().enumerate() {
        for j in 0..=i {
            store
                .append(t, s, &user_msg("r", &format!("e{j}"), j as i64))
                .await
                .unwrap();
        }
    }
    for (i, (t, s)) in pairs.iter().enumerate() {
        assert_eq!(
            store.read(t, s).await.unwrap().len(),
            i + 1,
            "leak/collide at {t}/{s}"
        );
    }
}

// ── serialization roundtrip ───────────────────────────────────────────────────

/// Every `SessionEvent` variant and every `ContentBlock` variant survives the
/// store byte-for-byte (asserted via canonical JSON value).
pub async fn event_payload_roundtrip_exact_all_variants(store: &dyn SessionStore) {
    let (t, s) = tenant_session();

    let assistant = Message::assistant_with_blocks(vec![
        ContentBlock::Text {
            text: "answer".into(),
            provider_metadata: Some(serde_json::json!({"k": "v"})),
        },
        ContentBlock::Thinking {
            thinking: "reasoning".into(),
            signature: Some("sig".into()),
            provider_metadata: None,
        },
        ContentBlock::RedactedThinking {
            data: "ENCRYPTED".into(),
        },
        ContentBlock::ToolUse {
            id: "tu1".into(),
            name: "search".into(),
            input: serde_json::json!({"q": "rust"}),
            provider_metadata: None,
        },
    ]);
    let tool_result = Message::user_with_blocks(vec![ContentBlock::ToolResult {
        tool_use_id: "tu1".into(),
        tool_name: "search".into(),
        content: "found".into(),
        is_error: false,
    }]);
    let media = Message::user_with_blocks(vec![
        ContentBlock::ArtifactRef {
            id: "art-1".into(),
            media_type: "application/pdf".into(),
            filename: Some("doc.pdf".into()),
        },
        ContentBlock::Image {
            media_type: "image/png".into(),
            data: "aW1n".into(),
        },
        ContentBlock::File {
            media_type: "text/plain".into(),
            data: "ZmlsZQ==".into(),
        },
    ]);

    let events = vec![
        run_start("r1", 0),
        SessionEvent::Message {
            run_id: "r1".into(),
            msg: Message::user("hi"),
            at: ts(1),
        },
        SessionEvent::Message {
            run_id: "r1".into(),
            msg: assistant,
            at: ts(2),
        },
        SessionEvent::Message {
            run_id: "r1".into(),
            msg: tool_result,
            at: ts(3),
        },
        SessionEvent::Message {
            run_id: "r1".into(),
            msg: media,
            at: ts(4),
        },
        SessionEvent::TurnBoundary {
            run_id: "r1".into(),
            at: ts(5),
        },
        SessionEvent::HookFired {
            run_id: "r1".into(),
            hook: "guard".into(),
            lifecycle: HookLifecycle::BeforeTool,
            hook_kind: "write".into(),
            outcome: "cancel".into(),
            note: Some("ok".into()),
            at: ts(6),
        },
        SessionEvent::StateSnapshot {
            run_id: "r1".into(),
            messages: vec![Message::user("compacted")],
            system_prompt: "sys".into(),
            reason: "compaction".into(),
            stats: None,
            open_tasks: None,
            data: None,
            at: ts(7),
        },
        SessionEvent::Message {
            run_id: "r1".into(),
            msg: Message::assistant("final").with_provider_msg_id("msg_anthropic_123"),
            at: ts(8),
        },
        run_end("r1", Some("end_turn"), 9),
    ];
    store.append_batch(&t, &s, &events).await.unwrap();

    let read = store.read(&t, &s).await.unwrap();
    assert_eq!(read.len(), events.len());
    for (orig, got) in events.iter().zip(read.iter()) {
        assert_eq!(
            value(orig),
            value(&got.event),
            "payload changed across roundtrip"
        );
    }
}

pub async fn timestamps_roundtrip_microsecond(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let at = ts(123_456); // µs precision
    store
        .append(
            &t,
            &s,
            &SessionEvent::Message {
                run_id: "r".into(),
                msg: Message::user("x"),
                at,
            },
        )
        .await
        .unwrap();
    let read = store.read(&t, &s).await.unwrap();
    match &read[0].event {
        SessionEvent::Message { at: got, .. } => assert_eq!(*got, at),
        _ => panic!("expected message"),
    }
}

pub async fn run_ids_roundtrip_exactly(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let run = "r-Wéird_99/xyz";
    store.append(&t, &s, &user_msg(run, "x", 0)).await.unwrap();
    assert_eq!(store.read(&t, &s).await.unwrap()[0].event.run_id(), run);
}

pub async fn large_text_payload_roundtrips(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let big = "z".repeat(200_000);
    store.append(&t, &s, &user_msg("r", &big, 0)).await.unwrap();
    assert_eq!(
        text_of(&store.read(&t, &s).await.unwrap()[0].event).as_deref(),
        Some(big.as_str())
    );
}

pub async fn unicode_payload_roundtrips(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    // NUL (`\u{0}`) is deliberately excluded: Postgres text/jsonb cannot store
    // it, so it's a backend-specific case (see each backend's own suite), not a
    // universal contract.
    let u = "héllo π 世界 🦀 \u{1F600} e\u{0301} \u{200D}tail";
    store.append(&t, &s, &user_msg("r", u, 0)).await.unwrap();
    assert_eq!(
        text_of(&store.read(&t, &s).await.unwrap()[0].event).as_deref(),
        Some(u)
    );
}

pub async fn empty_text_payload_roundtrips(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store.append(&t, &s, &user_msg("r", "", 0)).await.unwrap();
    let read = store.read(&t, &s).await.unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(text_of(&read[0].event).as_deref(), Some(""));
}

// ── listing & metadata ────────────────────────────────────────────────────────

pub async fn list_sessions_shows_appended_session(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store.append(&t, &s, &user_msg("r", "x", 0)).await.unwrap();
    let list = store.list_sessions(&t).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].session_id, s);
    assert_eq!(list[0].event_count, 1);
}

pub async fn list_sessions_is_tenant_scoped(store: &dyn SessionStore) {
    let (t1, t2) = (uid("tenant"), uid("tenant"));
    store
        .append(&t1, &uid("sess"), &user_msg("r", "x", 0))
        .await
        .unwrap();
    store
        .append(&t1, &uid("sess"), &user_msg("r", "y", 0))
        .await
        .unwrap();
    store
        .append(&t2, &uid("sess"), &user_msg("r", "z", 0))
        .await
        .unwrap();
    assert_eq!(store.list_sessions(&t1).await.unwrap().len(), 2);
    assert_eq!(store.list_sessions(&t2).await.unwrap().len(), 1);
    assert!(
        store
            .list_sessions(&uid("tenant"))
            .await
            .unwrap()
            .is_empty()
    );
}

pub async fn list_sessions_orders_recent_first(store: &dyn SessionStore) {
    let t = uid("tenant");
    let (a, b, c) = (uid("a"), uid("b"), uid("c"));
    // distinct, increasing last_activity via the event timestamps
    store.append(&t, &a, &user_msg("r", "x", 0)).await.unwrap();
    store
        .append(&t, &b, &user_msg("r", "x", 1_000_000))
        .await
        .unwrap();
    store
        .append(&t, &c, &user_msg("r", "x", 2_000_000))
        .await
        .unwrap();
    let order: Vec<String> = store
        .list_sessions(&t)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.session_id)
        .collect();
    assert_eq!(order, vec![c, b, a]); // most-recent-activity first
}

/// Keyset pagination over the thread list returns every session exactly once.
pub async fn list_sessions_page_covers_every_session_once(store: &dyn SessionStore) {
    let t = uid("tenant");
    let mut ids = Vec::new();
    for i in 0..12 {
        let s = uid("sess");
        // distinct last_activity so the (last_activity, id) cursor is unambiguous
        store
            .append(&t, &s, &user_msg("r", "x", i * 1_000_000))
            .await
            .unwrap();
        ids.push(s);
    }
    let mut seen = Vec::new();
    let mut cursor: Option<(DateTime<Utc>, String)> = None;
    loop {
        let page = store
            .list_sessions_page(&t, cursor.clone(), 5)
            .await
            .unwrap();
        if page.is_empty() {
            break;
        }
        let last = page.last().unwrap();
        cursor = Some((last.last_activity, last.session_id.clone()));
        seen.extend(page.into_iter().map(|m| m.session_id));
    }
    seen.sort();
    let mut want = ids.clone();
    want.sort();
    assert_eq!(seen, want, "pagination skipped, duplicated, or stalled");
}

pub async fn set_label_reflected_in_meta_and_list(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store.append(&t, &s, &user_msg("r", "x", 0)).await.unwrap();
    store.set_label(&t, &s, Some("My Thread")).await.unwrap();
    assert_eq!(
        store
            .session_meta(&t, &s)
            .await
            .unwrap()
            .unwrap()
            .label
            .as_deref(),
        Some("My Thread")
    );
    let listed = store.list_sessions(&t).await.unwrap();
    assert_eq!(listed[0].label.as_deref(), Some("My Thread"));
}

pub async fn set_label_materializes_empty_session(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .set_label(&t, &s, Some("Titled but empty"))
        .await
        .unwrap();
    let meta = store
        .session_meta(&t, &s)
        .await
        .unwrap()
        .expect("titled empty session is materialized");
    assert_eq!(meta.label.as_deref(), Some("Titled but empty"));
    assert_eq!(meta.event_count, 0);
    assert!(
        store
            .list_sessions(&t)
            .await
            .unwrap()
            .iter()
            .any(|m| m.session_id == s)
    );
}

pub async fn set_label_none_clears(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store.append(&t, &s, &user_msg("r", "x", 0)).await.unwrap();
    store.set_label(&t, &s, Some("temp")).await.unwrap();
    store.set_label(&t, &s, None).await.unwrap();
    assert_eq!(
        store.session_meta(&t, &s).await.unwrap().unwrap().label,
        None
    );
}

pub async fn set_label_does_not_disturb_event_count(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store.append(&t, &s, &user_msg("r", "a", 0)).await.unwrap();
    store.append(&t, &s, &user_msg("r", "b", 1)).await.unwrap();
    store.set_label(&t, &s, Some("label")).await.unwrap();
    let meta = store.session_meta(&t, &s).await.unwrap().unwrap();
    assert_eq!(meta.event_count, 2);
    assert_eq!(store.read(&t, &s).await.unwrap().len(), 2);
}

pub async fn delete_session_removes_from_read_and_list(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store.append(&t, &s, &user_msg("r", "x", 0)).await.unwrap();
    store.delete_session(&t, &s).await.unwrap();
    assert!(store.read(&t, &s).await.unwrap().is_empty());
    assert!(store.session_meta(&t, &s).await.unwrap().is_none());
    assert!(
        store
            .list_sessions(&t)
            .await
            .unwrap()
            .iter()
            .all(|m| m.session_id != s)
    );
}

pub async fn delete_session_is_tenant_scoped(store: &dyn SessionStore) {
    let (t1, t2) = (uid("tenant"), uid("tenant"));
    let s = uid("sess"); // same id under both tenants
    store.append(&t1, &s, &user_msg("r", "a", 0)).await.unwrap();
    store.append(&t2, &s, &user_msg("r", "b", 0)).await.unwrap();
    store.delete_session(&t1, &s).await.unwrap();
    assert!(store.read(&t1, &s).await.unwrap().is_empty());
    assert_eq!(
        store.read(&t2, &s).await.unwrap().len(),
        1,
        "deleting tenant1's thread hit tenant2"
    );
}

pub async fn recreate_after_delete_has_clean_log(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .append(&t, &s, &user_msg("r", "old1", 0))
        .await
        .unwrap();
    store
        .append(&t, &s, &user_msg("r", "old2", 1))
        .await
        .unwrap();
    store.delete_session(&t, &s).await.unwrap();
    let seq = store
        .append(&t, &s, &user_msg("r", "new", 0))
        .await
        .unwrap();
    assert_eq!(seq, 1, "seq must restart on a recreated session");
    let read = store.read(&t, &s).await.unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(text_of(&read[0].event).as_deref(), Some("new"));
}

pub async fn session_meta_absent_for_unknown(store: &dyn SessionStore) {
    let (t, _) = tenant_session();
    assert!(
        store
            .session_meta(&t, &uid("never"))
            .await
            .unwrap()
            .is_none()
    );
}

// ── run reconstruction (replay) ───────────────────────────────────────────────

pub async fn reconstruct_completed_run(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .append_batch(
            &t,
            &s,
            &[
                run_start("r1", 0),
                user_msg("r1", "question", 1),
                assistant_msg("r1", "the answer", 2),
                run_end("r1", Some("end_turn"), 3),
            ],
        )
        .await
        .unwrap();
    let state = replay_into_state(store, &t, &s, "sys").await.unwrap();
    assert_eq!(state.user_id, t);
    let runs = state.runs();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].started_at.is_some() && runs[0].ended_at.is_some());
    assert!(
        state.current_run().is_none(),
        "a completed run is not in-flight"
    );
    assert_eq!(state.last_assistant_text().as_deref(), Some("the answer"));
}

pub async fn reconstruct_in_flight_run(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .append_batch(&t, &s, &[run_start("r1", 0), user_msg("r1", "working", 1)])
        .await
        .unwrap();
    let state = replay_into_state(store, &t, &s, "sys").await.unwrap();
    let cur = state
        .current_run()
        .expect("RunStart with no RunEnd is in-flight");
    assert_eq!(cur.id, "r1");
    assert!(cur.ended_at.is_none());
}

pub async fn reconstruct_terminal_run_preserves_stop_reason(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .append_batch(
            &t,
            &s,
            &[run_start("r1", 0), run_end("r1", Some("cancelled"), 1)],
        )
        .await
        .unwrap();
    let state = replay_into_state(store, &t, &s, "sys").await.unwrap();
    assert!(
        state.current_run().is_none(),
        "a run with RunEnd is terminal"
    );
    let stop = state.events().iter().find_map(|e| match e {
        SessionEvent::RunEnd { outcome, .. } => outcome.stop_reason.clone(),
        _ => None,
    });
    assert_eq!(stop.as_deref(), Some("cancelled"));
}

pub async fn reconstruct_multiple_runs_in_order(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .append_batch(
            &t,
            &s,
            &[
                run_start("r1", 0),
                run_end("r1", Some("end_turn"), 1),
                run_start("r2", 2),
                run_end("r2", Some("end_turn"), 3),
            ],
        )
        .await
        .unwrap();
    let state = replay_into_state(store, &t, &s, "sys").await.unwrap();
    let runs = state.runs();
    let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["r1", "r2"]);
}

pub async fn reconstruct_tool_call_and_result_messages(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let call = Message::assistant_with_blocks(vec![ContentBlock::ToolUse {
        id: "tu1".into(),
        name: "lookup".into(),
        input: serde_json::json!({"x": 1}),
        provider_metadata: None,
    }]);
    let result = Message::user_with_blocks(vec![ContentBlock::ToolResult {
        tool_use_id: "tu1".into(),
        tool_name: "lookup".into(),
        content: "42".into(),
        is_error: false,
    }]);
    store
        .append_batch(
            &t,
            &s,
            &[
                SessionEvent::Message {
                    run_id: "r1".into(),
                    msg: call.clone(),
                    at: ts(0),
                },
                SessionEvent::Message {
                    run_id: "r1".into(),
                    msg: result.clone(),
                    at: ts(1),
                },
            ],
        )
        .await
        .unwrap();

    let msgs = replay_messages(store, &t, &s).await.unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(
        serde_json::to_value(&msgs[0]).unwrap(),
        serde_json::to_value(&call).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&msgs[1]).unwrap(),
        serde_json::to_value(&result).unwrap()
    );
}

pub async fn run_rows_lifecycle(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .create_run(&t, &s, "r-1", "coral", &Default::default())
        .await
        .unwrap();

    let rec = store.get_run(&t, "r-1").await.unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Pending);
    assert_eq!(rec.agent, "coral");
    assert_eq!(rec.session_id, s);

    store
        .set_run_status("r-1", RunStatus::Running, None)
        .await
        .unwrap();
    assert_eq!(
        store.get_run(&t, "r-1").await.unwrap().unwrap().status,
        RunStatus::Running
    );

    store
        .set_run_status("r-1", RunStatus::Error, Some("provider died"))
        .await
        .unwrap();
    let rec = store.get_run(&t, "r-1").await.unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Error);
    assert_eq!(rec.error.as_deref(), Some("provider died"));
    assert!(rec.status.is_terminal());
}

pub async fn pause_resume_cycle(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .create_run(&t, &s, "r-pause", "coral", &Default::default())
        .await
        .unwrap();
    store
        .set_run_status("r-pause", RunStatus::Running, None)
        .await
        .unwrap();
    store
        .set_run_status("r-pause", RunStatus::Paused, None)
        .await
        .unwrap();
    assert!(!RunStatus::Paused.is_terminal());

    assert!(store.resume_run(&t, "r-pause").await.unwrap());
    let rec = store.get_run(&t, "r-pause").await.unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Queued);
    assert!(rec.claimed_by.is_none());

    assert!(!store.resume_run(&t, "r-pause").await.unwrap());

    store
        .set_run_status("r-pause", RunStatus::Paused, None)
        .await
        .unwrap();
    assert!(!store.resume_run("intruder", "r-pause").await.unwrap());
    assert_eq!(
        store.get_run(&t, "r-pause").await.unwrap().unwrap().status,
        RunStatus::Paused
    );

    assert!(store.request_cancel_run(&t, "r-pause").await.unwrap());

    assert!(!store.resume_run(&t, "ghost").await.unwrap());
    store
        .create_run(&t, &s, "r-done", "coral", &Default::default())
        .await
        .unwrap();
    store
        .set_run_status("r-done", RunStatus::Running, None)
        .await
        .unwrap();
    assert!(!store.resume_run(&t, "r-done").await.unwrap());
    store
        .set_run_status("r-done", RunStatus::Success, None)
        .await
        .unwrap();
    assert!(!store.resume_run(&t, "r-done").await.unwrap());
    assert_eq!(
        store.get_run(&t, "r-done").await.unwrap().unwrap().status,
        RunStatus::Success
    );
}

pub async fn cancelling_a_paused_run_prevents_resume(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let r = uid("paused-cancel");
    store
        .create_run(&t, &s, &r, "coral", &Default::default())
        .await
        .unwrap();
    store
        .set_run_status(&r, RunStatus::Paused, None)
        .await
        .unwrap();

    assert!(store.request_cancel_run(&t, &r).await.unwrap());
    assert!(
        !store.resume_run(&t, &r).await.unwrap(),
        "a user-cancelled paused run must not be resumable"
    );
    assert_eq!(
        store.get_run(&t, &r).await.unwrap().unwrap().status,
        RunStatus::Cancelled,
        "cancelling a paused run should make it terminal"
    );
}

pub async fn resume_clears_a_stale_paused_lease(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let r = uid("paused-lease");
    store
        .create_run(&t, &s, &r, "coral", &Default::default())
        .await
        .unwrap();
    assert!(
        store
            .claim_run(&r, "old-worker", chrono::Duration::seconds(60))
            .await
            .unwrap()
    );
    store
        .set_run_status(&r, RunStatus::Paused, None)
        .await
        .unwrap();

    assert!(store.resume_run(&t, &r).await.unwrap());
    let rec = store.get_run(&t, &r).await.unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Queued);
    assert!(rec.claimed_by.is_none());
    assert!(rec.lease_expires_at.is_none());
    assert!(
        store
            .claim_run(&r, "new-worker", chrono::Duration::seconds(60))
            .await
            .unwrap(),
        "resumed runs should be claimable by a fresh worker"
    );
}

pub async fn latest_run_picks_the_newest(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .create_run(&t, &s, "r-old", "coral", &Default::default())
        .await
        .unwrap();
    store
        .set_run_status("r-old", RunStatus::Success, None)
        .await
        .unwrap();
    store
        .create_run(&t, &s, "r-new", "scout", &Default::default())
        .await
        .unwrap();

    let latest = store.latest_run(&t, &s).await.unwrap().unwrap();
    assert_eq!(latest.run_id, "r-new");
    assert_eq!(latest.status, RunStatus::Pending);
}

pub async fn runs_are_tenant_scoped(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .create_run(&t, &s, "r-mine", "coral", &Default::default())
        .await
        .unwrap();
    assert!(
        store
            .get_run("someone-else", "r-mine")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .latest_run("someone-else", &s)
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.get_run(&t, "r-missing").await.unwrap().is_none());
}

pub async fn claiming_a_pending_run_takes_the_lease_once(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let r = uid("r");
    store
        .create_run(&t, &s, &r, "coral", &Default::default())
        .await
        .unwrap();

    let lease = chrono::Duration::seconds(30);
    assert!(store.claim_run(&r, "instance-a", lease).await.unwrap());
    let rec = store.get_run(&t, &r).await.unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Running);
    assert_eq!(rec.claimed_by.as_deref(), Some("instance-a"));
    assert!(rec.lease_expires_at.unwrap() > chrono::Utc::now());

    assert!(!store.claim_run(&r, "instance-b", lease).await.unwrap());
    let rec = store.get_run(&t, &r).await.unwrap().unwrap();
    assert_eq!(rec.claimed_by.as_deref(), Some("instance-a"));

    assert!(
        !store
            .claim_run(&uid("r-missing"), "instance-a", lease)
            .await
            .unwrap()
    );
}

pub async fn heartbeat_extends_the_lease_for_the_owner_only(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let r = uid("r");
    store
        .create_run(&t, &s, &r, "coral", &Default::default())
        .await
        .unwrap();
    store
        .claim_run(&r, "instance-a", chrono::Duration::seconds(1))
        .await
        .unwrap();
    let short = store
        .get_run(&t, &r)
        .await
        .unwrap()
        .unwrap()
        .lease_expires_at
        .unwrap();

    let lease = chrono::Duration::seconds(60);
    assert!(
        store
            .heartbeat_run(&r, "instance-b", lease)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .heartbeat_run(&r, "instance-a", lease)
            .await
            .unwrap()
            .is_some()
    );
    let extended = store
        .get_run(&t, &r)
        .await
        .unwrap()
        .unwrap()
        .lease_expires_at
        .unwrap();
    assert!(extended > short);

    store
        .set_run_status(&r, RunStatus::Success, None)
        .await
        .unwrap();
    assert!(
        store
            .heartbeat_run(&r, "instance-a", lease)
            .await
            .unwrap()
            .is_none()
    );
}

pub async fn reaping_marks_only_expired_running_runs(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let (dead, alive, idle, paused) = (
        uid("r-dead"),
        uid("r-alive"),
        uid("r-idle"),
        uid("r-paused"),
    );
    store
        .create_run(&t, &s, &dead, "coral", &Default::default())
        .await
        .unwrap();
    store
        .create_run(&t, &s, &alive, "coral", &Default::default())
        .await
        .unwrap();
    store
        .create_run(&t, &s, &idle, "coral", &Default::default())
        .await
        .unwrap();
    store
        .create_run(&t, &s, &paused, "coral", &Default::default())
        .await
        .unwrap();
    store
        .set_run_status(&paused, RunStatus::Paused, None)
        .await
        .unwrap();
    store
        .claim_run(&dead, "instance-a", chrono::Duration::seconds(-1))
        .await
        .unwrap();
    store
        .claim_run(&alive, "instance-a", chrono::Duration::seconds(60))
        .await
        .unwrap();

    let reaped = store.reap_expired_runs().await.unwrap();
    assert!(reaped.iter().any(|r| r.run_id == dead));
    assert!(!reaped.iter().any(|r| r.run_id == alive));
    assert!(!reaped.iter().any(|r| r.run_id == idle));
    assert!(
        !reaped.iter().any(|r| r.run_id == paused),
        "the reaper must never touch a paused run"
    );

    let rec = store.get_run(&t, &dead).await.unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Error);
    assert_eq!(rec.error.as_deref(), Some("lease expired"));
    assert_eq!(
        store.get_run(&t, &alive).await.unwrap().unwrap().status,
        RunStatus::Running
    );
    assert_eq!(
        store.get_run(&t, &idle).await.unwrap().unwrap().status,
        RunStatus::Pending
    );
    assert_eq!(
        store.get_run(&t, &paused).await.unwrap().unwrap().status,
        RunStatus::Paused
    );

    let again = store.reap_expired_runs().await.unwrap();
    assert!(!again.iter().any(|r| r.run_id == dead));
}

pub async fn deleting_a_session_deletes_its_runs(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .create_run(&t, &s, "r-1", "coral", &Default::default())
        .await
        .unwrap();
    store.delete_session(&t, &s).await.unwrap();
    assert!(store.get_run(&t, "r-1").await.unwrap().is_none());
}

pub async fn queued_runs_dequeue_oldest_first_and_release_requeues(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let (a, b) = (uid("q-a"), uid("q-b"));
    store
        .create_run(
            &t,
            &s,
            &a,
            "coral",
            &runic_substrate::RunInput {
                input: Some(serde_json::json!("first")),
                context: None,
                queued: true,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    store
        .create_run(
            &t,
            &s,
            &b,
            "coral",
            &runic_substrate::RunInput {
                input: Some(serde_json::json!("second")),
                context: Some(serde_json::json!({"k": "v"})),
                queued: true,
            },
        )
        .await
        .unwrap();

    let rec = store.get_run(&t, &b).await.unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Queued);
    assert_eq!(rec.input, Some(serde_json::json!("second")));
    assert_eq!(rec.context, Some(serde_json::json!({"k": "v"})));

    let lease = chrono::Duration::seconds(60);
    let mut mine = Vec::new();
    for _ in 0..50 {
        match store
            .claim_next_queued_run("inst-test", lease)
            .await
            .unwrap()
        {
            Some(r) if r.run_id == a || r.run_id == b => {
                mine.push(r.run_id.clone());
                if mine.len() == 2 {
                    break;
                }
            }
            Some(_) => continue,
            None => break,
        }
    }
    assert_eq!(mine, vec![a.clone(), b.clone()], "oldest queued run first");
    assert_eq!(
        store.get_run(&t, &a).await.unwrap().unwrap().status,
        RunStatus::Running
    );

    store.release_run(&a, "inst-test").await.unwrap();
    let rec = store.get_run(&t, &a).await.unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Queued);
    assert!(rec.claimed_by.is_none());

    store.release_run(&b, "inst-other").await.unwrap();
    assert_eq!(
        store.get_run(&t, &b).await.unwrap().unwrap().status,
        RunStatus::Running,
        "release by a non-owner is a no-op"
    );
}

pub async fn thread_leases_fence_claim_extend_release(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let lease = chrono::Duration::seconds(60);

    assert!(store.claim_thread(&t, &s, "inst-a", lease).await.unwrap());
    assert!(!store.claim_thread(&t, &s, "inst-b", lease).await.unwrap());
    assert!(store.claim_thread(&t, &s, "inst-a", lease).await.unwrap());

    assert!(
        !store
            .extend_thread_lease(&t, &s, "inst-b", lease)
            .await
            .unwrap()
    );
    assert!(
        store
            .extend_thread_lease(&t, &s, "inst-a", lease)
            .await
            .unwrap()
    );

    store.release_thread(&t, &s, "inst-b").await.unwrap();
    assert!(!store.claim_thread(&t, &s, "inst-b", lease).await.unwrap());
    store.release_thread(&t, &s, "inst-a").await.unwrap();
    assert!(store.claim_thread(&t, &s, "inst-b", lease).await.unwrap());

    let (t2, s2) = tenant_session();
    assert!(
        store
            .claim_thread(&t2, &s2, "inst-a", chrono::Duration::seconds(-1))
            .await
            .unwrap()
    );
    assert!(store.claim_thread(&t2, &s2, "inst-b", lease).await.unwrap());
}

pub async fn thread_leases_are_tenant_and_session_scoped(store: &dyn SessionStore) {
    let tenant = uid("tenant");
    let session = uid("session");
    let lease = chrono::Duration::seconds(60);

    assert!(
        store
            .claim_thread(&tenant, &session, "inst-a", lease)
            .await
            .unwrap()
    );
    assert!(
        store
            .claim_thread(&format!("{tenant}-other"), &session, "inst-b", lease)
            .await
            .unwrap(),
        "same session id in another tenant must not be fenced"
    );
    assert!(
        store
            .claim_thread(&tenant, &format!("{session}-other"), "inst-b", lease)
            .await
            .unwrap(),
        "another session in the same tenant must not be fenced"
    );

    store
        .release_thread(&format!("{tenant}-other"), &session, "inst-b")
        .await
        .unwrap();
    assert!(
        !store
            .claim_thread(&tenant, &session, "inst-c", lease)
            .await
            .unwrap(),
        "releasing another tenant's lease must not release this tenant"
    );
    store
        .release_thread(&tenant, &format!("{session}-other"), "inst-b")
        .await
        .unwrap();
    assert!(
        !store
            .claim_thread(&tenant, &session, "inst-c", lease)
            .await
            .unwrap(),
        "releasing another session's lease must not release this session"
    );
}

pub async fn cancel_and_steering_signals_flow_through_the_heartbeat(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let r = uid("r");
    store
        .create_run(&t, &s, &r, "coral", &Default::default())
        .await
        .unwrap();
    let lease = chrono::Duration::seconds(60);
    assert!(store.claim_run(&r, "inst-a", lease).await.unwrap());

    assert!(store.push_steering(&t, &r, "focus on rust").await.unwrap());
    assert!(store.push_steering(&t, &r, "be brief").await.unwrap());
    assert!(store.request_cancel_run(&t, &r).await.unwrap());

    let signals = store
        .heartbeat_run(&r, "inst-a", lease)
        .await
        .unwrap()
        .unwrap();
    assert!(signals.cancel_requested);
    assert_eq!(signals.steering, ["focus on rust", "be brief"]);

    let signals = store
        .heartbeat_run(&r, "inst-a", lease)
        .await
        .unwrap()
        .unwrap();
    assert!(signals.cancel_requested);
    assert!(signals.steering.is_empty(), "steering drains exactly once");

    assert!(
        !store
            .request_cancel_run(&t, &uid("r-missing"))
            .await
            .unwrap()
    );
    store
        .set_run_status(&r, RunStatus::Success, None)
        .await
        .unwrap();
    assert!(!store.push_steering(&t, &r, "too late").await.unwrap());
    assert!(!store.request_cancel_run(&t, &r).await.unwrap());
}

pub async fn cancelling_an_unclaimed_queued_run_drops_it(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let r = uid("q");
    store
        .create_run(
            &t,
            &s,
            &r,
            "coral",
            &runic_substrate::RunInput {
                input: Some(serde_json::json!("go")),
                context: None,
                queued: true,
            },
        )
        .await
        .unwrap();

    assert!(store.request_cancel_run(&t, &r).await.unwrap());
    let rec = store.get_run(&t, &r).await.unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Cancelled);
    assert!(!rec.cancel_requested, "dropped, not flagged");
}

pub async fn cancelling_a_pending_run_flags_it_for_the_owner(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let r = uid("pending");
    let lease = chrono::Duration::seconds(60);
    store
        .create_run(&t, &s, &r, "coral", &Default::default())
        .await
        .unwrap();

    assert!(store.request_cancel_run(&t, &r).await.unwrap());
    let rec = store.get_run(&t, &r).await.unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Pending);
    assert!(rec.cancel_requested);

    assert!(store.claim_run(&r, "inst-a", lease).await.unwrap());
    let signals = store
        .heartbeat_run(&r, "inst-a", lease)
        .await
        .unwrap()
        .unwrap();
    assert!(signals.cancel_requested);
}

pub async fn cancelled_queued_runs_are_not_dequeued(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let cancelled = uid("q-cancelled");
    let live = uid("q-live");
    store
        .create_run(
            &t,
            &s,
            &cancelled,
            "coral",
            &runic_substrate::RunInput {
                input: Some(serde_json::json!("cancel me")),
                context: None,
                queued: true,
            },
        )
        .await
        .unwrap();
    store
        .create_run(
            &t,
            &s,
            &live,
            "coral",
            &runic_substrate::RunInput {
                input: Some(serde_json::json!("run me")),
                context: None,
                queued: true,
            },
        )
        .await
        .unwrap();

    assert!(store.request_cancel_run(&t, &cancelled).await.unwrap());

    let mut claimed = Vec::new();
    for _ in 0..10 {
        if let Some(run) = store
            .claim_next_queued_run("inst-a", chrono::Duration::seconds(60))
            .await
            .unwrap()
        {
            claimed.push(run.run_id);
        }
    }

    assert_eq!(claimed, vec![live]);
    assert_eq!(
        store.get_run(&t, &cancelled).await.unwrap().unwrap().status,
        RunStatus::Cancelled
    );
}

pub async fn steering_is_rejected_after_a_queued_run_is_cancelled(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let r = uid("queued");
    store
        .create_run(
            &t,
            &s,
            &r,
            "coral",
            &runic_substrate::RunInput {
                input: Some(serde_json::json!("go")),
                context: None,
                queued: true,
            },
        )
        .await
        .unwrap();

    assert!(store.request_cancel_run(&t, &r).await.unwrap());
    assert!(!store.push_steering(&t, &r, "too late").await.unwrap());
    let rec = store.get_run(&t, &r).await.unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Cancelled);
}

pub async fn terminal_runs_reject_late_signals(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    for status in [RunStatus::Success, RunStatus::Error, RunStatus::Cancelled] {
        let r = uid("terminal");
        store
            .create_run(&t, &s, &r, "coral", &Default::default())
            .await
            .unwrap();
        store
            .set_run_status(&r, status, Some("done"))
            .await
            .unwrap();

        assert!(!store.request_cancel_run(&t, &r).await.unwrap());
        assert!(!store.push_steering(&t, &r, "too late").await.unwrap());
        assert!(
            store
                .heartbeat_run(&r, "inst-a", chrono::Duration::seconds(60))
                .await
                .unwrap()
                .is_none()
        );
    }
}

pub async fn signal_operations_are_tenant_scoped(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let r = uid("r");
    let lease = chrono::Duration::seconds(60);
    store
        .create_run(&t, &s, &r, "coral", &Default::default())
        .await
        .unwrap();
    assert!(store.claim_run(&r, "inst-a", lease).await.unwrap());

    assert!(!store.request_cancel_run("other-tenant", &r).await.unwrap());
    assert!(
        !store
            .push_steering("other-tenant", &r, "foreign nudge")
            .await
            .unwrap()
    );

    let signals = store
        .heartbeat_run(&r, "inst-a", lease)
        .await
        .unwrap()
        .unwrap();
    assert!(!signals.cancel_requested);
    assert!(signals.steering.is_empty());
}

pub async fn wrong_heartbeat_owner_does_not_drain_signals(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let r = uid("r");
    let lease = chrono::Duration::seconds(60);
    store
        .create_run(&t, &s, &r, "coral", &Default::default())
        .await
        .unwrap();
    assert!(store.claim_run(&r, "inst-a", lease).await.unwrap());
    assert!(store.push_steering(&t, &r, "keep me").await.unwrap());
    assert!(store.request_cancel_run(&t, &r).await.unwrap());

    assert!(
        store
            .heartbeat_run(&r, "inst-b", lease)
            .await
            .unwrap()
            .is_none(),
        "a non-owner heartbeat must not see or drain signals"
    );
    let signals = store
        .heartbeat_run(&r, "inst-a", lease)
        .await
        .unwrap()
        .unwrap();
    assert!(signals.cancel_requested);
    assert_eq!(signals.steering, ["keep me"]);
}

pub async fn multiple_steering_messages_keep_fifo_order(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let r = uid("r");
    let lease = chrono::Duration::seconds(60);
    store
        .create_run(&t, &s, &r, "coral", &Default::default())
        .await
        .unwrap();
    assert!(store.claim_run(&r, "inst-a", lease).await.unwrap());

    for i in 0..20 {
        assert!(
            store
                .push_steering(&t, &r, &format!("nudge-{i:02}"))
                .await
                .unwrap()
        );
    }
    let signals = store
        .heartbeat_run(&r, "inst-a", lease)
        .await
        .unwrap()
        .unwrap();
    let expected: Vec<String> = (0..20).map(|i| format!("nudge-{i:02}")).collect();
    assert_eq!(signals.steering, expected);
}

pub async fn delete_session_clears_run_signals_and_thread_lease(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let r = uid("r");
    let lease = chrono::Duration::seconds(60);
    store
        .create_run(&t, &s, &r, "coral", &Default::default())
        .await
        .unwrap();
    assert!(store.claim_run(&r, "inst-a", lease).await.unwrap());
    assert!(store.claim_thread(&t, &s, "inst-a", lease).await.unwrap());
    assert!(store.push_steering(&t, &r, "orphan me").await.unwrap());
    assert!(store.request_cancel_run(&t, &r).await.unwrap());

    store.delete_session(&t, &s).await.unwrap();

    assert!(store.get_run(&t, &r).await.unwrap().is_none());
    assert!(!store.request_cancel_run(&t, &r).await.unwrap());
    assert!(!store.push_steering(&t, &r, "too late").await.unwrap());
    assert!(
        store.claim_thread(&t, &s, "inst-b", lease).await.unwrap(),
        "deleting a thread must not leave a stale thread lease behind"
    );
}

pub async fn delete_session_does_not_clear_other_tenant_thread_lease(store: &dyn SessionStore) {
    let session = uid("shared-session");
    let lease = chrono::Duration::seconds(60);
    assert!(
        store
            .claim_thread("tenant-a", &session, "inst-a", lease)
            .await
            .unwrap()
    );
    assert!(
        store
            .claim_thread("tenant-b", &session, "inst-b", lease)
            .await
            .unwrap()
    );

    store.delete_session("tenant-a", &session).await.unwrap();

    assert!(
        store
            .claim_thread("tenant-a", &session, "inst-c", lease)
            .await
            .unwrap(),
        "deleted tenant's lease should be gone"
    );
    assert!(
        !store
            .claim_thread("tenant-b", &session, "inst-c", lease)
            .await
            .unwrap(),
        "deleting tenant-a must not release tenant-b's lease"
    );
}

pub async fn pending_and_paused_runs_are_never_dequeued(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let pending = uid("r-pending");
    let paused = uid("r-paused-dq");
    store
        .create_run(&t, &s, &pending, "coral", &Default::default())
        .await
        .unwrap();
    store
        .create_run(&t, &s, &paused, "coral", &Default::default())
        .await
        .unwrap();
    store
        .set_run_status(&paused, RunStatus::Paused, None)
        .await
        .unwrap();

    for _ in 0..50 {
        match store
            .claim_next_queued_run("inst-test", chrono::Duration::seconds(60))
            .await
            .unwrap()
        {
            Some(rec) => {
                assert_ne!(rec.run_id, pending, "a pending run was dequeued");
                assert_ne!(rec.run_id, paused, "a paused run was dequeued");
            }
            None => break,
        }
    }
    assert_eq!(
        store.get_run(&t, &pending).await.unwrap().unwrap().status,
        RunStatus::Pending
    );
    assert_eq!(
        store.get_run(&t, &paused).await.unwrap().unwrap().status,
        RunStatus::Paused
    );
}

pub async fn read_tail_starts_at_the_last_snapshot(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .append_batch(
            &t,
            &s,
            &[
                user_msg("r1", "old", 0),
                SessionEvent::StateSnapshot {
                    run_id: "r1".into(),
                    messages: vec![Message::user("first snap")],
                    system_prompt: "sys".into(),
                    reason: "compaction".into(),
                    stats: None,
                    open_tasks: None,
                    data: None,
                    at: ts(1),
                },
                user_msg("r2", "middle", 2),
                SessionEvent::StateSnapshot {
                    run_id: "r2".into(),
                    messages: vec![Message::user("second snap")],
                    system_prompt: "sys".into(),
                    reason: "compaction".into(),
                    stats: None,
                    open_tasks: None,
                    data: None,
                    at: ts(3),
                },
                user_msg("r3", "tail", 4),
            ],
        )
        .await
        .unwrap();

    let tail = store.read_tail(&t, &s).await.unwrap();
    assert_eq!(tail.len(), 2);
    assert!(matches!(tail[0].event, SessionEvent::StateSnapshot { .. }));
    match &tail[1].event {
        SessionEvent::Message { msg, .. } => assert_eq!(msg.content.text_content(), "tail"),
        other => panic!("expected the tail message, got {other:?}"),
    }
}

pub async fn read_tail_without_a_snapshot_reads_everything(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .append_batch(&t, &s, &[user_msg("r1", "a", 0), user_msg("r1", "b", 1)])
        .await
        .unwrap();
    let tail = store.read_tail(&t, &s).await.unwrap();
    assert_eq!(tail.len(), 2);
}

pub async fn snapshot_replaces_messages_on_replay(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    store
        .append_batch(
            &t,
            &s,
            &[
                user_msg("r1", "old one", 0),
                user_msg("r1", "old two", 1),
                SessionEvent::StateSnapshot {
                    run_id: "r1".into(),
                    messages: vec![Message::user("compacted")],
                    system_prompt: "sys".into(),
                    reason: "compaction".into(),
                    stats: None,
                    open_tasks: None,
                    data: None,
                    at: ts(2),
                },
                assistant_msg("r1", "after", 3),
            ],
        )
        .await
        .unwrap();
    let msgs = replay_messages(store, &t, &s).await.unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].content.text_content(), "compacted");
    assert_eq!(msgs[1].content.text_content(), "after");
}

// ── search (optional; opt-in backends) ────────────────────────────────────────

async fn seed_chat(store: &dyn SessionStore, tenant: &str, session: &str, text: &str) {
    store
        .append(tenant, session, &user_msg("r", text, 0))
        .await
        .unwrap();
}

pub async fn search_finds_other_session_same_tenant(store: &dyn SessionStore) {
    let t = uid("tenant");
    let (past, current) = (uid("past"), uid("current"));
    seed_chat(store, &t, &past, "the deployment finished cleanly").await;
    seed_chat(store, &t, &current, "unrelated chatter").await;
    let hits = store
        .search(&t, "deployment", 10, Some(&current))
        .await
        .unwrap();
    assert!(hits.iter().any(|h| h.session_id == past));
}

pub async fn search_excludes_current_session(store: &dyn SessionStore) {
    let t = uid("tenant");
    let current = uid("current");
    seed_chat(store, &t, &current, "deployment happening here").await;
    let hits = store
        .search(&t, "deployment", 10, Some(&current))
        .await
        .unwrap();
    assert!(hits.iter().all(|h| h.session_id != current));
}

pub async fn search_is_tenant_scoped(store: &dyn SessionStore) {
    let (t1, t2) = (uid("tenant"), uid("tenant"));
    seed_chat(store, &t2, &uid("s"), "secret deployment in another tenant").await;
    let hits = store.search(&t1, "deployment", 10, None).await.unwrap();
    assert!(hits.is_empty(), "search crossed tenants");
}

pub async fn search_respects_limit(store: &dyn SessionStore) {
    let t = uid("tenant");
    for _ in 0..5 {
        seed_chat(store, &t, &uid("s"), "deployment deployment").await;
    }
    assert!(store.search(&t, "deployment", 2, None).await.unwrap().len() <= 2);
}

pub async fn search_empty_when_no_match(store: &dyn SessionStore) {
    let t = uid("tenant");
    seed_chat(store, &t, &uid("s"), "nothing relevant").await;
    assert!(
        store
            .search(&t, "zzqqxx", 10, None)
            .await
            .unwrap()
            .is_empty()
    );
}
