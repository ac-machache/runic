use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use proptest::prelude::*;
use runic_e2e_harness::dummy_agents;
use runic_serve::{ServeConfig, WorkerConfig, router};
use runic_substrate::{
    ArtifactStore, MemoryArtifactStore, MemorySessionStore, RunStatus, SessionEvent, SessionStore,
    StoredEvent,
};
use runic_types::{ContentBlock, MessageContent};
use serde_json::json;
use tower::ServiceExt;

#[derive(Debug, Clone)]
enum Directive {
    Add(i32, i32),
    Say(String),
    Echo(String),
    Fail,
    Slow(u16),
    Skill,
    Delegate,
}

impl Directive {
    fn to_text(&self) -> String {
        match self {
            Directive::Add(a, b) => format!("add:{a},{b}"),
            Directive::Say(s) => format!("say:{s}"),
            Directive::Echo(s) => format!("echo:{s}"),
            Directive::Fail => "fail".to_string(),
            Directive::Slow(ms) => format!("slow:{ms}"),
            Directive::Skill => "skill:task".to_string(),
            Directive::Delegate => "delegate".to_string(),
        }
    }
}

fn simple_directive() -> impl Strategy<Value = Directive> {
    prop_oneof![
        (-999i32..999, -999i32..999).prop_map(|(a, b)| Directive::Add(a, b)),
        "[a-z]{1,6}".prop_map(Directive::Say),
        "[a-z]{1,6}".prop_map(Directive::Echo),
        Just(Directive::Fail),
        (0u16..30).prop_map(Directive::Slow),
        Just(Directive::Skill),
        Just(Directive::Delegate),
    ]
}

#[derive(Debug, Clone)]
struct Job {
    tenant: u8,
    dir: Directive,
    on_b: bool,
}

fn job() -> impl Strategy<Value = Job> {
    (0u8..2, simple_directive(), any::<bool>()).prop_map(|(tenant, dir, on_b)| Job {
        tenant,
        dir,
        on_b,
    })
}

fn tenant_id(prefix: &str, t: u8) -> String {
    format!("{prefix}t{t}")
}

fn post(uri: &str, tenant: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-runic-tenant", tenant)
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn wait_until(
    store: &dyn SessionStore,
    tenant: &str,
    run_id: &str,
    pred: impl Fn(RunStatus) -> bool,
) -> Option<RunStatus> {
    for _ in 0..300 {
        if let Some(rec) = store.get_run(tenant, run_id).await.unwrap()
            && pred(rec.status)
        {
            return Some(rec.status);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    store
        .get_run(tenant, run_id)
        .await
        .unwrap()
        .map(|r| r.status)
}

fn count_starts(evs: &[StoredEvent], run_id: &str) -> usize {
    evs.iter()
        .filter(|e| matches!(&e.event, SessionEvent::RunStart { run_id: r, .. } if r == run_id))
        .count()
}

fn count_ends(evs: &[StoredEvent], run_id: &str) -> usize {
    evs.iter()
        .filter(|e| matches!(&e.event, SessionEvent::RunEnd { run_id: r, .. } if r == run_id))
        .count()
}

fn assert_gapless(evs: &[StoredEvent], th: &str) -> Result<(), TestCaseError> {
    for (i, e) in evs.iter().enumerate() {
        prop_assert_eq!(e.seq, i as u64 + 1, "seq gap in thread {}", th);
    }
    Ok(())
}

fn has_dangling(evs: &[StoredEvent]) -> bool {
    let mut uses = std::collections::HashSet::new();
    let mut results = std::collections::HashSet::new();
    for e in evs {
        if let SessionEvent::Message { msg, .. } = &e.event
            && let MessageContent::Blocks(b) = &msg.content
        {
            for x in b {
                match x {
                    ContentBlock::ToolUse { id, .. } => {
                        uses.insert(id.clone());
                    }
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        results.insert(tool_use_id.clone());
                    }
                    _ => {}
                }
            }
        }
    }
    !uses.is_subset(&results)
}

fn two_instances(
    store: Arc<dyn SessionStore>,
    artifacts: Arc<dyn ArtifactStore>,
) -> (axum::Router, axum::Router) {
    let mk = || {
        router(
            ServeConfig::new(store.clone(), artifacts.clone(), dummy_agents(false)).workers(
                WorkerConfig {
                    max_concurrent_runs: 4,
                    poll_every: Duration::from_millis(10),
                },
            ),
        )
    };
    (mk(), mk())
}

async fn run_concurrent(
    store: Arc<dyn SessionStore>,
    prefix: &str,
    jobs: Vec<Job>,
) -> Result<(), TestCaseError> {
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifactStore::new());
    let (app_a, app_b) = two_instances(store.clone(), artifacts);

    let mut handles = Vec::new();
    for (i, job) in jobs.iter().enumerate() {
        let app = if job.on_b {
            app_b.clone()
        } else {
            app_a.clone()
        };
        let t = tenant_id(prefix, job.tenant);
        let th = format!("{prefix}cth{i}");
        let text = job.dir.to_text();
        handles.push(tokio::spawn(async move {
            let resp = app
                .oneshot(post(
                    &format!("/threads/{th}/runs"),
                    &t,
                    json!({ "message": text }),
                ))
                .await
                .unwrap();
            let status = resp.status();
            let run_id = body_json(resp).await["run_id"]
                .as_str()
                .unwrap_or("")
                .to_string();
            (t, th, status, run_id)
        }));
    }

    let mut started = Vec::new();
    for h in handles {
        started.push(h.await.unwrap());
    }

    for (job, (t, th, status, run_id)) in jobs.iter().zip(started.iter()) {
        prop_assert_eq!(*status, StatusCode::ACCEPTED, "background run not accepted");
        prop_assert!(!run_id.is_empty(), "no run_id in accepted response");

        let final_status = wait_until(store.as_ref(), t, run_id, |s| s.is_terminal()).await;
        prop_assert_eq!(
            final_status,
            Some(RunStatus::Success),
            "job on thread {} did not reach Success",
            th
        );

        let evs = store.read(t, th).await.unwrap();
        let starts = count_starts(&evs, run_id);
        let ends = count_ends(&evs, run_id);
        prop_assert_eq!(
            starts,
            1,
            "run {} started {} times (double claim)",
            run_id,
            starts
        );
        prop_assert_eq!(
            ends,
            1,
            "run {} ended {} times (double execute)",
            run_id,
            ends
        );
        assert_gapless(&evs, th)?;

        let other = tenant_id(prefix, (job.tenant + 1) % 2);
        prop_assert!(
            store.get_run(&other, run_id).await.unwrap().is_none(),
            "run {} leaked to tenant {}",
            run_id,
            other
        );
    }
    Ok(())
}

async fn resume_across_instances(
    store: Arc<dyn SessionStore>,
    prefix: &str,
    question: String,
    start_on_b: bool,
    answer_on_b: bool,
) -> Result<(), TestCaseError> {
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifactStore::new());
    let (app_a, app_b) = two_instances(store.clone(), artifacts);
    let start = if start_on_b { &app_b } else { &app_a };
    let answer = if answer_on_b { &app_b } else { &app_a };

    let t = tenant_id(prefix, 0);
    let th = format!("{prefix}resume-th");

    let resp = start
        .clone()
        .oneshot(post(
            &format!("/threads/{th}/runs"),
            &t,
            json!({ "message": format!("ask:{question}") }),
        ))
        .await
        .unwrap();
    prop_assert_eq!(resp.status(), StatusCode::ACCEPTED, "ask run not accepted");
    let run_id = body_json(resp).await["run_id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    prop_assert!(!run_id.is_empty(), "no run_id for ask");

    let paused = wait_until(store.as_ref(), &t, &run_id, |s| {
        matches!(s, RunStatus::Paused | RunStatus::Success | RunStatus::Error)
    })
    .await;
    prop_assert_eq!(paused, Some(RunStatus::Paused), "ask run did not pause");

    let evs = store.read(&t, &th).await.unwrap();
    prop_assert!(
        evs.iter().any(|e| matches!(&e.event,
            SessionEvent::ToolDeferred { run_id: r, .. } if *r == run_id)),
        "no ToolDeferred for paused run"
    );

    let ans = answer
        .clone()
        .oneshot(post(
            &format!("/threads/{th}/asks/call-1"),
            &t,
            json!({ "answer": "resolved" }),
        ))
        .await
        .unwrap();
    prop_assert_eq!(ans.status(), StatusCode::ACCEPTED, "answer not accepted");

    let done = wait_until(store.as_ref(), &t, &run_id, |s| s.is_terminal()).await;
    prop_assert_eq!(
        done,
        Some(RunStatus::Success),
        "run did not resume to Success across instances"
    );

    let evs = store.read(&t, &th).await.unwrap();
    prop_assert_eq!(
        count_starts(&evs, &run_id),
        1,
        "resumed run has a second RunStart"
    );
    prop_assert_eq!(
        count_ends(&evs, &run_id),
        1,
        "resumed run RunEnd count wrong"
    );
    assert_gapless(&evs, &th)?;
    Ok(())
}

async fn run_same_thread(
    store: Arc<dyn SessionStore>,
    prefix: &str,
    dirs: Vec<Directive>,
) -> Result<(), TestCaseError> {
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifactStore::new());
    let (app_a, app_b) = two_instances(store.clone(), artifacts);

    let t = tenant_id(prefix, 0);
    let th = format!("{prefix}shared-th");

    let mut handles = Vec::new();
    for (i, dir) in dirs.iter().enumerate() {
        let app = if i % 2 == 0 {
            app_a.clone()
        } else {
            app_b.clone()
        };
        let (t, th, text) = (t.clone(), th.clone(), dir.to_text());
        handles.push(tokio::spawn(async move {
            let resp = app
                .oneshot(post(
                    &format!("/threads/{th}/runs"),
                    &t,
                    json!({ "message": text }),
                ))
                .await
                .unwrap();
            let status = resp.status();
            let run_id = body_json(resp).await["run_id"]
                .as_str()
                .unwrap_or("")
                .to_string();
            (status, run_id)
        }));
    }

    let mut run_ids = Vec::new();
    for h in handles {
        let (status, run_id) = h.await.unwrap();
        prop_assert_eq!(status, StatusCode::ACCEPTED, "same-thread run not accepted");
        prop_assert!(!run_id.is_empty(), "no run_id");
        run_ids.push(run_id);
    }

    for run_id in &run_ids {
        let final_status = wait_until(store.as_ref(), &t, run_id, |s| s.is_terminal()).await;
        prop_assert_eq!(
            final_status,
            Some(RunStatus::Success),
            "run {} not Success",
            run_id
        );
    }

    let evs = store.read(&t, &th).await.unwrap();
    assert_gapless(&evs, &th)?;
    prop_assert!(!has_dangling(&evs), "dangling tool call on shared thread");
    for run_id in &run_ids {
        prop_assert_eq!(count_starts(&evs, run_id), 1, "run {} start count", run_id);
        prop_assert_eq!(count_ends(&evs, run_id), 1, "run {} end count", run_id);
    }

    let mut open: Option<String> = None;
    for e in &evs {
        match &e.event {
            SessionEvent::RunStart { run_id, .. } => {
                prop_assert!(
                    open.is_none(),
                    "run {} started while {:?} still open — threads not serialized",
                    run_id,
                    open
                );
                open = Some(run_id.clone());
            }
            SessionEvent::RunEnd { run_id, .. } => {
                prop_assert_eq!(
                    open.as_deref(),
                    Some(run_id.as_str()),
                    "RunEnd for {} but open run is {:?}",
                    run_id,
                    open
                );
                open = None;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn run_cancel_race(
    store: Arc<dyn SessionStore>,
    prefix: &str,
    slow_ms: u16,
    cancel_on_b: bool,
) -> Result<(), TestCaseError> {
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifactStore::new());
    let (app_a, app_b) = two_instances(store.clone(), artifacts);

    let t = tenant_id(prefix, 0);
    let th = format!("{prefix}cancel-th");

    let resp = app_a
        .clone()
        .oneshot(post(
            &format!("/threads/{th}/runs"),
            &t,
            json!({ "message": format!("slow:{slow_ms}") }),
        ))
        .await
        .unwrap();
    prop_assert_eq!(resp.status(), StatusCode::ACCEPTED, "slow run not accepted");
    let run_id = body_json(resp).await["run_id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    prop_assert!(!run_id.is_empty(), "no run_id");

    let cancel_app = if cancel_on_b { &app_b } else { &app_a };
    let _ = cancel_app
        .clone()
        .oneshot(post(&format!("/threads/{th}/runs/cancel"), &t, json!({})))
        .await
        .unwrap();

    let status = wait_until(store.as_ref(), &t, &run_id, |s| s.is_terminal()).await;
    prop_assert!(
        matches!(
            status,
            Some(RunStatus::Success) | Some(RunStatus::Cancelled)
        ),
        "cancel race left run in {:?}, not a terminal state",
        status
    );

    let evs = store.read(&t, &th).await.unwrap();
    assert_gapless(&evs, &th)?;
    prop_assert!(
        count_starts(&evs, &run_id) <= 1,
        "run started more than once"
    );
    prop_assert!(count_ends(&evs, &run_id) <= 1, "run ended more than once");
    if status == Some(RunStatus::Success) {
        prop_assert!(
            !has_dangling(&evs),
            "successful run left a dangling tool call"
        );
    }
    Ok(())
}

fn mem() -> Arc<dyn SessionStore> {
    Arc::new(MemorySessionStore::new())
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, max_shrink_iters: 3000, ..ProptestConfig::default() })]

    #[test]
    fn concurrent_runs_across_two_instances_stay_consistent(
        jobs in prop::collection::vec(job(), 1..8)
    ) {
        rt().block_on(run_concurrent(mem(), "", jobs))?;
    }

    #[test]
    fn paused_run_resumes_on_a_different_instance(
        question in "[a-z]{1,6}",
        start_on_b in any::<bool>(),
        answer_on_b in any::<bool>(),
    ) {
        rt().block_on(resume_across_instances(mem(), "", question, start_on_b, answer_on_b))?;
    }

    #[test]
    fn same_thread_runs_serialize(dirs in prop::collection::vec(simple_directive(), 2..6)) {
        rt().block_on(run_same_thread(mem(), "", dirs))?;
    }

    #[test]
    fn cancel_races_stay_consistent(
        slow_ms in 0u16..200,
        cancel_on_b in any::<bool>(),
    ) {
        rt().block_on(run_cancel_race(mem(), "", slow_ms, cancel_on_b))?;
    }
}

mod pg {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use runic_substrate::PostgresSessionStore;

    static CASE: AtomicU64 = AtomicU64::new(0);

    async fn store() -> Option<Arc<dyn SessionStore>> {
        let url = std::env::var("RUNIC_TEST_DATABASE_URL").ok()?;
        let s = PostgresSessionStore::connect(&url)
            .await
            .expect("RUNIC_TEST_DATABASE_URL unreachable");
        Some(Arc::new(s))
    }

    fn prefix() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("cc{}-{}-", nanos, CASE.fetch_add(1, Ordering::SeqCst))
    }

    fn jobs() -> Vec<Job> {
        vec![
            Job {
                tenant: 0,
                dir: Directive::Add(2, 3),
                on_b: false,
            },
            Job {
                tenant: 1,
                dir: Directive::Echo("x".into()),
                on_b: true,
            },
            Job {
                tenant: 0,
                dir: Directive::Fail,
                on_b: true,
            },
            Job {
                tenant: 1,
                dir: Directive::Slow(20),
                on_b: false,
            },
        ]
    }

    #[test]
    fn postgres_concurrent_runs_stay_consistent() {
        rt().block_on(async {
            let Some(s) = store().await else {
                eprintln!("skipped: RUNIC_TEST_DATABASE_URL unset");
                return;
            };
            run_concurrent(s, &prefix(), jobs()).await.unwrap();
        });
    }

    #[test]
    fn postgres_same_thread_serializes() {
        rt().block_on(async {
            let Some(s) = store().await else { return };
            let dirs = vec![
                Directive::Add(1, 1),
                Directive::Say("a".into()),
                Directive::Slow(15),
                Directive::Echo("z".into()),
            ];
            run_same_thread(s, &prefix(), dirs).await.unwrap();
        });
    }

    #[test]
    fn postgres_paused_resumes_across_instances() {
        rt().block_on(async {
            let Some(s) = store().await else { return };
            resume_across_instances(s, &prefix(), "why".into(), true, false)
                .await
                .unwrap();
        });
    }

    #[test]
    fn postgres_reaper_marks_dead_worker_runs() {
        rt().block_on(async {
            let Some(s) = store().await else { return };
            let p = prefix();
            let (t, th, rid) = (format!("{p}t"), format!("{p}th"), format!("{p}r"));
            s.create_run(&t, &th, &rid, "main", &Default::default())
                .await
                .unwrap();
            let claimed = s
                .claim_run(&rid, "dead-inst", chrono::Duration::milliseconds(50))
                .await
                .unwrap();
            assert!(
                claimed,
                "could not claim the run for the dead-worker scenario"
            );

            let reaper = runic_serve::spawn_lease_reaper(s.clone(), Duration::from_millis(20));
            let mut final_status = None;
            for _ in 0..200 {
                if let Some(rec) = s.get_run(&t, &rid).await.unwrap()
                    && rec.status.is_terminal()
                {
                    final_status = Some(rec.status);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            reaper.abort();
            assert_eq!(
                final_status,
                Some(RunStatus::Error),
                "reaper did not mark the dead-worker run as Error"
            );
        });
    }
}
