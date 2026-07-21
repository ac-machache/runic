use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use proptest::prelude::*;
use runic_e2e_harness::dummy_agents;
use runic_serve::{ServeConfig, WorkerConfig, router};
use runic_substrate::{
    ArtifactStore, MemoryArtifactStore, MemorySessionStore, RunStatus, SessionEvent, SessionStore,
};
use serde_json::json;
use tower::ServiceExt;

#[derive(Debug, Clone)]
enum Directive {
    Add(i32, i32),
    Say(String),
    Echo(String),
    Fail,
    Slow(u16),
    Ask(String),
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
            Directive::Ask(q) => format!("ask:{q}"),
            Directive::Skill => "skill:task".to_string(),
            Directive::Delegate => "delegate".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
enum Action {
    CreateThread {
        tenant: u8,
        thread: u8,
    },
    Run {
        tenant: u8,
        thread: u8,
        dir: Directive,
    },
    RunBackground {
        tenant: u8,
        thread: u8,
        dir: Directive,
    },
    CancelPaused {
        tenant: u8,
        thread: u8,
    },
}

fn tenant_id(t: u8) -> String {
    format!("t{t}")
}
fn thread_id(t: u8) -> String {
    format!("th{t}")
}

fn simple_directive() -> impl Strategy<Value = Directive> {
    prop_oneof![
        (-999i32..999, -999i32..999).prop_map(|(a, b)| Directive::Add(a, b)),
        "[a-z]{1,6}".prop_map(Directive::Say),
        "[a-z]{1,6}".prop_map(Directive::Echo),
        Just(Directive::Fail),
        (0u16..40).prop_map(Directive::Slow),
        Just(Directive::Skill),
        Just(Directive::Delegate),
    ]
}

fn directive() -> impl Strategy<Value = Directive> {
    prop_oneof![simple_directive(), "[a-z]{1,6}".prop_map(Directive::Ask)]
}

const TENANTS: [&str; 2] = ["t0", "t1"];
const THREADS: [&str; 3] = ["th0", "th1", "th2"];

async fn check_structure(store: &MemorySessionStore) -> Result<(), TestCaseError> {
    for t in TENANTS {
        for th in THREADS {
            let evs = store.read(t, th).await.unwrap();
            let mut seen = std::collections::HashSet::new();
            for e in &evs {
                let SessionEvent::RunStart { run_id, .. } = &e.event else {
                    continue;
                };
                if !seen.insert(run_id.clone()) {
                    continue;
                }
                let starts = evs
                    .iter()
                    .filter(|x| matches!(&x.event, SessionEvent::RunStart { run_id: r, .. } if r == run_id))
                    .count();
                let ends = evs
                    .iter()
                    .filter(|x| matches!(&x.event, SessionEvent::RunEnd { run_id: r, .. } if r == run_id))
                    .count();
                prop_assert_eq!(starts, 1, "run {} has {} RunStarts", run_id, starts);
                prop_assert!(ends <= 1, "run {} has {} RunEnds", run_id, ends);
                if let Some(rec) = store.get_run(t, run_id).await.unwrap() {
                    match rec.status {
                        RunStatus::Success | RunStatus::Error => {
                            prop_assert_eq!(ends, 1, "run {} missing RunEnd", run_id);
                        }
                        RunStatus::Paused => {
                            prop_assert_eq!(ends, 0, "paused run {} has a RunEnd", run_id);
                        }
                        _ => {}
                    }
                }
                let hook_fired = evs.iter().any(|x| {
                    matches!(&x.event,
                    SessionEvent::StateUpdated { run_id: r, key, .. }
                        if r == run_id && key == "hook_fired")
                });
                prop_assert!(
                    hook_fired,
                    "before_agent hook did not fire for run {}",
                    run_id
                );
            }
        }
    }
    Ok(())
}

async fn wait_for_success(
    store: &MemorySessionStore,
    tenant: &str,
    run_id: &str,
) -> Option<RunStatus> {
    for _ in 0..200 {
        if let Some(rec) = store.get_run(tenant, run_id).await.unwrap()
            && rec.status.is_terminal()
        {
            return Some(rec.status);
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    store
        .get_run(tenant, run_id)
        .await
        .unwrap()
        .map(|r| r.status)
}

fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        1 => (0u8..2, 0u8..3).prop_map(|(tenant, thread)| Action::CreateThread { tenant, thread }),
        4 => (0u8..2, 0u8..3, directive())
            .prop_map(|(tenant, thread, dir)| Action::Run { tenant, thread, dir }),
        3 => (0u8..2, 0u8..3, simple_directive())
            .prop_map(|(tenant, thread, dir)| Action::RunBackground { tenant, thread, dir }),
        2 => (0u8..2, 0u8..3)
            .prop_map(|(tenant, thread)| Action::CancelPaused { tenant, thread }),
    ]
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

async fn run_sequence(actions: Vec<Action>) -> Result<(), TestCaseError> {
    let store = Arc::new(MemorySessionStore::new());
    let session: Arc<dyn SessionStore> = store.clone();
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(MemoryArtifactStore::new());
    let app = router(
        ServeConfig::new(session, artifacts, dummy_agents(false)).workers(WorkerConfig {
            max_concurrent_runs: 4,
            poll_every: Duration::from_millis(15),
        }),
    );

    for act in actions {
        match act {
            Action::CreateThread { tenant, thread } => {
                let (t, th) = (tenant_id(tenant), thread_id(thread));
                let resp = app
                    .clone()
                    .oneshot(post("/threads", &t, json!({ "thread_id": th })))
                    .await
                    .unwrap();
                prop_assert!(
                    resp.status().as_u16() < 500,
                    "create thread 5xx: {}",
                    resp.status()
                );
            }
            Action::Run {
                tenant,
                thread,
                dir,
            } => {
                let (t, th) = (tenant_id(tenant), thread_id(thread));
                let resp = app
                    .clone()
                    .oneshot(post(
                        &format!("/threads/{th}/runs/wait"),
                        &t,
                        json!({ "message": dir.to_text() }),
                    ))
                    .await
                    .unwrap();
                prop_assert_eq!(resp.status(), StatusCode::OK, "run not OK for {:?}", dir);
                let body = body_json(resp).await;
                let run_id = body["run_id"].as_str().unwrap_or("").to_string();
                prop_assert!(!run_id.is_empty(), "no run_id in {body}");

                if matches!(dir, Directive::Ask(_)) {
                    prop_assert_eq!(
                        body["stop_reason"].as_str(),
                        Some("suspended"),
                        "ask did not suspend"
                    );
                    prop_assert!(
                        matches!(
                            store.get_run(&t, &run_id).await.unwrap().map(|r| r.status),
                            Some(RunStatus::Paused)
                        ),
                        "ask run not Paused"
                    );
                    let evs = store.read(&t, &th).await.unwrap();
                    prop_assert!(
                        evs.iter().any(|e| matches!(&e.event,
                            SessionEvent::ToolDeferred { run_id: r, .. } if *r == run_id)),
                        "no ToolDeferred for paused run"
                    );
                    prop_assert!(
                        !evs.iter().any(|e| matches!(&e.event,
                            SessionEvent::RunEnd { run_id: r, .. } if *r == run_id)),
                        "RunEnd present before answer"
                    );

                    let other = tenant_id((tenant + 1) % 2);
                    let wrong = app
                        .clone()
                        .oneshot(post(
                            &format!("/threads/{th}/asks/call-1"),
                            &other,
                            json!({ "answer": "no" }),
                        ))
                        .await
                        .unwrap();
                    prop_assert_eq!(
                        wrong.status(),
                        StatusCode::BAD_REQUEST,
                        "wrong-tenant answer accepted"
                    );
                    prop_assert!(
                        matches!(
                            store.get_run(&t, &run_id).await.unwrap().map(|r| r.status),
                            Some(RunStatus::Paused)
                        ),
                        "wrong-tenant answer disturbed the paused run"
                    );

                    let ok = app
                        .clone()
                        .oneshot(post(
                            &format!("/threads/{th}/asks/call-1"),
                            &t,
                            json!({ "answer": "yes" }),
                        ))
                        .await
                        .unwrap();
                    prop_assert_eq!(ok.status(), StatusCode::ACCEPTED, "answer not accepted");

                    let final_status = wait_for_success(store.as_ref(), &t, &run_id).await;
                    prop_assert_eq!(
                        final_status,
                        Some(RunStatus::Success),
                        "ask run did not resume to Success"
                    );
                    let evs2 = store.read(&t, &th).await.unwrap();
                    prop_assert!(
                        evs2.iter().any(|e| matches!(&e.event,
                            SessionEvent::RunEnd { run_id: r, .. } if *r == run_id)),
                        "no RunEnd after resume"
                    );
                } else {
                    prop_assert_eq!(
                        body["stop_reason"].as_str(),
                        Some("end_turn"),
                        "unexpected stop_reason for {:?}",
                        dir
                    );
                    if let Directive::Add(a, b) = dir {
                        let want = (a as i64 + b as i64).to_string();
                        let got = body["text"].as_str().unwrap_or("");
                        prop_assert!(
                            got.contains(&want),
                            "add {a}+{b} -> text {got:?} missing {want}"
                        );
                    }
                    let rec = store.get_run(&t, &run_id).await.unwrap();
                    prop_assert!(
                        matches!(rec.as_ref().map(|r| r.status), Some(RunStatus::Success)),
                        "run {run_id} not Success: {:?}",
                        rec.map(|r| r.status)
                    );
                }

                let other = tenant_id((tenant + 1) % 2);
                prop_assert!(
                    store.get_run(&other, &run_id).await.unwrap().is_none(),
                    "run {run_id} leaked to tenant {other}"
                );
                let evs = store.read(&t, &th).await.unwrap();
                for (i, e) in evs.iter().enumerate() {
                    prop_assert_eq!(e.seq, i as u64 + 1, "seq gap in {}/{}", t, th);
                }
            }
            Action::RunBackground {
                tenant,
                thread,
                dir,
            } => {
                let (t, th) = (tenant_id(tenant), thread_id(thread));
                let resp = app
                    .clone()
                    .oneshot(post(
                        &format!("/threads/{th}/runs"),
                        &t,
                        json!({ "message": dir.to_text() }),
                    ))
                    .await
                    .unwrap();
                prop_assert_eq!(
                    resp.status(),
                    StatusCode::ACCEPTED,
                    "background run not accepted for {:?}",
                    dir
                );
                let run_id = body_json(resp).await["run_id"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                prop_assert!(!run_id.is_empty(), "background run has no run_id");
                let status = wait_for_success(store.as_ref(), &t, &run_id).await;
                prop_assert_eq!(
                    status,
                    Some(RunStatus::Success),
                    "background run {:?} did not reach Success: {:?}",
                    dir,
                    status
                );
                let other = tenant_id((tenant + 1) % 2);
                prop_assert!(
                    store.get_run(&other, &run_id).await.unwrap().is_none(),
                    "background run leaked to tenant {other}"
                );
                let evs = store.read(&t, &th).await.unwrap();
                for (i, e) in evs.iter().enumerate() {
                    prop_assert_eq!(e.seq, i as u64 + 1, "seq gap in {}/{}", t, th);
                }
            }
            Action::CancelPaused { tenant, thread } => {
                let (t, th) = (tenant_id(tenant), thread_id(thread));
                let resp = app
                    .clone()
                    .oneshot(post(
                        &format!("/threads/{th}/runs/wait"),
                        &t,
                        json!({ "message": "ask:x" }),
                    ))
                    .await
                    .unwrap();
                prop_assert_eq!(resp.status(), StatusCode::OK);
                let run_id = body_json(resp).await["run_id"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                prop_assert!(
                    matches!(
                        store.get_run(&t, &run_id).await.unwrap().map(|r| r.status),
                        Some(RunStatus::Paused)
                    ),
                    "ask run not paused before cancel"
                );

                let cancel = app
                    .clone()
                    .oneshot(post(&format!("/threads/{th}/runs/cancel"), &t, json!({})))
                    .await
                    .unwrap();
                prop_assert_eq!(
                    cancel.status(),
                    StatusCode::ACCEPTED,
                    "cancel of a paused run not accepted"
                );
                prop_assert_eq!(
                    store.get_run(&t, &run_id).await.unwrap().map(|r| r.status),
                    Some(RunStatus::Cancelled),
                    "paused run not cancelled"
                );

                let answer = app
                    .clone()
                    .oneshot(post(
                        &format!("/threads/{th}/asks/call-1"),
                        &t,
                        json!({ "answer": "too late" }),
                    ))
                    .await
                    .unwrap();
                prop_assert_eq!(
                    answer.status(),
                    StatusCode::BAD_REQUEST,
                    "answered a cancelled run"
                );
                prop_assert_eq!(
                    store.get_run(&t, &run_id).await.unwrap().map(|r| r.status),
                    Some(RunStatus::Cancelled),
                    "cancelled run left cancelled after rejected answer"
                );
            }
        }
        check_structure(&store).await?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, max_shrink_iters: 4000, ..ProptestConfig::default() })]

    #[test]
    fn lifecycle_invariants_hold(actions in prop::collection::vec(action(), 1..16)) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_sequence(actions))?;
    }
}
