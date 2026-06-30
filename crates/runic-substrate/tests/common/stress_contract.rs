//! Volume/stress correctness — enough scale to catch O(n²) folds and
//! pagination skip/dup bugs, without pretending to be a benchmark. Ignored by
//! default; run with `cargo test -- --ignored`.

use chrono::{DateTime, Utc};

use runic_state::SessionEvent;
use runic_substrate::{ArtifactSource, ArtifactStore, SessionStore, StoredEvent, replay_messages};
use runic_types::Message;

use crate::common::ids::{tenant_session, uid};

fn ts(n: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(1_700_000_000_000_000 + n).unwrap()
}

fn msg(text: &str, n: i64) -> SessionEvent {
    SessionEvent::Message {
        run_id: "r".into(),
        msg: Message::user(text),
        at: ts(n),
    }
}

async fn paginate(store: &dyn SessionStore, t: &str, s: &str, page: usize) -> Vec<StoredEvent> {
    let mut out = Vec::new();
    let mut after = 0u64;
    loop {
        let batch = store.read_after_limited(t, s, after, page).await.unwrap();
        if batch.is_empty() {
            break;
        }
        after = batch.last().unwrap().seq;
        out.extend(batch);
    }
    out
}

pub async fn ten_thousand_events_one_thread(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let n = 10_000i64;
    // append in chunks so a single batch stays reasonable for any backend
    for chunk in 0..10 {
        let events: Vec<SessionEvent> = (0..1000)
            .map(|i| {
                let g = chunk * 1000 + i;
                msg(&format!("m{g}"), g)
            })
            .collect();
        store.append_batch(&t, &s, &events).await.unwrap();
    }
    assert_eq!(store.read(&t, &s).await.unwrap().len(), n as usize);

    let walked = paginate(store, &t, &s, 250).await;
    let seqs: Vec<u64> = walked.iter().map(|e| e.seq).collect();
    assert_eq!(
        seqs,
        (1..=n as u64).collect::<Vec<_>>(),
        "pagination skip/dup at scale"
    );

    assert_eq!(
        replay_messages(store, &t, &s).await.unwrap().len(),
        n as usize
    );
}

pub async fn one_thousand_threads_one_tenant(store: &dyn SessionStore) {
    let t = uid("tenant");
    for i in 0..1000 {
        let s = uid("sess");
        store.append(&t, &s, &msg("x", i)).await.unwrap();
    }
    assert_eq!(store.list_sessions(&t).await.unwrap().len(), 1000);

    // page the thread list with a small window; every thread appears once
    let mut seen = std::collections::HashSet::new();
    let mut cursor: Option<(DateTime<Utc>, String)> = None;
    loop {
        let page = store
            .list_sessions_page(&t, cursor.clone(), 50)
            .await
            .unwrap();
        if page.is_empty() {
            break;
        }
        let last = page.last().unwrap();
        cursor = Some((last.last_activity, last.session_id.clone()));
        for m in page {
            assert!(seen.insert(m.session_id), "thread listed twice");
        }
    }
    assert_eq!(seen.len(), 1000, "thread pagination skipped or stalled");
}

pub async fn hundred_tenants_same_thread_name(store: &dyn SessionStore) {
    let s = uid("shared-sess"); // SAME id under 100 tenants
    let tenants: Vec<String> = (0..100).map(|_| uid("tenant")).collect();
    for (i, t) in tenants.iter().enumerate() {
        for j in 0..=(i % 5) {
            store
                .append(t, &s, &msg(&format!("e{j}"), j as i64))
                .await
                .unwrap();
        }
    }
    for (i, t) in tenants.iter().enumerate() {
        assert_eq!(
            store.read(t, &s).await.unwrap().len(),
            (i % 5) + 1,
            "tenant bleed at {t}"
        );
    }
}

pub async fn append_batch_sizes_1_10_100_1000(store: &dyn SessionStore) {
    for size in [1usize, 10, 100, 1000] {
        let (t, s) = tenant_session();
        let events: Vec<SessionEvent> =
            (0..size).map(|i| msg(&format!("m{i}"), i as i64)).collect();
        store.append_batch(&t, &s, &events).await.unwrap();
        let seqs: Vec<u64> = store
            .read(&t, &s)
            .await
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(
            seqs,
            (1..=size as u64).collect::<Vec<_>>(),
            "batch size {size}"
        );
    }
}

pub async fn paginate_two_thousand_events_small_page(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let events: Vec<SessionEvent> = (0..2000).map(|i| msg(&format!("m{i}"), i)).collect();
    store.append_batch(&t, &s, &events).await.unwrap();
    let walked = paginate(store, &t, &s, 13).await;
    let seqs: Vec<u64> = walked.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=2000u64).collect::<Vec<_>>());
}

pub async fn reconstruct_large_log(store: &dyn SessionStore) {
    let (t, s) = tenant_session();
    let mut events = vec![SessionEvent::RunStart {
        run_id: "r".into(),
        at: ts(0),
    }];
    events.extend((0..5000).map(|i| msg(&format!("m{i}"), 1 + i)));
    store.append_batch(&t, &s, &events).await.unwrap();
    let msgs = replay_messages(store, &t, &s).await.unwrap();
    assert_eq!(
        msgs.len(),
        5000,
        "RunStart is not a message; 5000 messages fold"
    );
}

pub async fn one_thousand_artifacts_one_session(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    let mut ids = std::collections::HashSet::new();
    for i in 0..1000 {
        let a = store
            .put(
                &t,
                &s,
                "text/plain",
                ArtifactSource::UserUpload,
                format!("n{i}").as_bytes(),
            )
            .await
            .unwrap();
        assert!(ids.insert(a.id), "duplicate artifact id at scale");
    }
    assert_eq!(store.list(&t, &s).await.unwrap().len(), 1000);
}
