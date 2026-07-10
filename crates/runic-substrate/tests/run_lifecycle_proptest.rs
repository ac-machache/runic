use std::collections::HashMap;

use chrono::Duration;
use proptest::prelude::*;
use runic_substrate::{MemorySessionStore, RunStatus, SessionStore};
use tokio::runtime::Runtime;

fn runs_for(prefix: &str) -> [(String, String); 4] {
    [
        (format!("{prefix}r0"), format!("{prefix}t0")),
        (format!("{prefix}r1"), format!("{prefix}t0")),
        (format!("{prefix}r2"), format!("{prefix}t1")),
        (format!("{prefix}r3"), format!("{prefix}t1")),
    ]
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct M {
    status: RunStatus,
    owner: Option<usize>,
}

fn terminal(s: RunStatus) -> bool {
    matches!(
        s,
        RunStatus::Success | RunStatus::Error | RunStatus::Cancelled
    )
}

#[derive(Debug, Clone)]
enum Op {
    Claim(usize, usize),
    ClaimNext(usize),
    Release(usize, usize),
    Complete(usize),
    Pause(usize),
    Resume(usize),
    Cancel(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0usize..4, 0usize..2).prop_map(|(r, w)| Op::Claim(r, w)),
        (0usize..2).prop_map(Op::ClaimNext),
        (0usize..4, 0usize..2).prop_map(|(r, w)| Op::Release(r, w)),
        (0usize..4).prop_map(Op::Complete),
        (0usize..4).prop_map(Op::Pause),
        (0usize..4).prop_map(Op::Resume),
        (0usize..4).prop_map(Op::Cancel),
    ]
}

async fn run_model(
    store: &dyn SessionStore,
    runs: &[(String, String); 4],
    allow_next: bool,
    ops: Vec<Op>,
) -> Result<(), TestCaseError> {
    let worker = |w: usize| format!("{}w{w}", runs[0].0);
    let id_to_idx = |id: &str| runs.iter().position(|(r, _)| r == id);
    let t0 = runs[0].1.clone();
    let t1 = runs[2].1.clone();
    let wrong = |i: usize| {
        if runs[i].1 == t0 {
            t1.clone()
        } else {
            t0.clone()
        }
    };

    let mut model: HashMap<usize, M> = HashMap::new();
    for (i, (r, t)) in runs.iter().enumerate() {
        store
            .create_run(t, "s", r, "coral", &Default::default())
            .await
            .unwrap();
        model.insert(
            i,
            M {
                status: RunStatus::Pending,
                owner: None,
            },
        );
    }
    let lease = Duration::seconds(3600);

    for op in ops {
        match op {
            Op::Claim(ri, wi) => {
                let m = model.get_mut(&ri).unwrap();
                let expect =
                    matches!(m.status, RunStatus::Pending | RunStatus::Queued) && m.owner.is_none();
                let got = store
                    .claim_run(&runs[ri].0, &worker(wi), lease)
                    .await
                    .unwrap();
                prop_assert_eq!(got, expect, "claim r{} by w{}", ri, wi);
                if expect {
                    m.status = RunStatus::Running;
                    m.owner = Some(wi);
                }
            }
            Op::ClaimNext(wi) => {
                if !allow_next {
                    continue;
                }
                let got = store
                    .claim_next_queued_run(&worker(wi), lease)
                    .await
                    .unwrap();
                match got {
                    Some(rec) => {
                        let ri =
                            id_to_idx(&rec.run_id).expect("claim_next handed out a foreign run");
                        let m = model.get_mut(&ri).unwrap();
                        prop_assert!(
                            m.status == RunStatus::Queued && m.owner.is_none(),
                            "claim_next handed out non-candidate r{}",
                            ri
                        );
                        m.status = RunStatus::Running;
                        m.owner = Some(wi);
                    }
                    None => {
                        let any = model
                            .values()
                            .any(|m| m.status == RunStatus::Queued && m.owner.is_none());
                        prop_assert!(!any, "claim_next returned None but a queued run exists");
                    }
                }
            }
            Op::Release(ri, wi) => {
                store.release_run(&runs[ri].0, &worker(wi)).await.unwrap();
                let m = model.get_mut(&ri).unwrap();
                if m.owner == Some(wi) {
                    m.status = RunStatus::Queued;
                    m.owner = None;
                }
            }
            Op::Complete(ri) => {
                store
                    .set_run_status(&runs[ri].0, RunStatus::Success, None)
                    .await
                    .unwrap();
                model.get_mut(&ri).unwrap().status = RunStatus::Success;
            }
            Op::Pause(ri) => {
                store
                    .set_run_status(&runs[ri].0, RunStatus::Paused, None)
                    .await
                    .unwrap();
                model.get_mut(&ri).unwrap().status = RunStatus::Paused;
            }
            Op::Resume(ri) => {
                let m = model.get_mut(&ri).unwrap();
                let expect = m.status == RunStatus::Paused;
                let got = store.resume_run(&runs[ri].1, &runs[ri].0).await.unwrap();
                prop_assert_eq!(got, expect, "resume r{}", ri);
                if expect {
                    m.status = RunStatus::Queued;
                    m.owner = None;
                }
            }
            Op::Cancel(ri) => {
                let m = model.get_mut(&ri).unwrap();
                let got = store
                    .request_cancel_run(&runs[ri].1, &runs[ri].0)
                    .await
                    .unwrap();
                if terminal(m.status) {
                    prop_assert!(!got, "cancel of terminal r{} returned true", ri);
                } else {
                    prop_assert!(got, "cancel of non-terminal r{} returned false", ri);
                    let dormant = m.status == RunStatus::Paused
                        || (m.status == RunStatus::Queued && m.owner.is_none());
                    if dormant {
                        m.status = RunStatus::Cancelled;
                    }
                }
            }
        }

        for (i, (r, t)) in runs.iter().enumerate() {
            let rec = store.get_run(t, r).await.unwrap().unwrap();
            let m = &model[&i];
            prop_assert_eq!(rec.status, m.status, "status divergence on r{}", i);
            let owner = rec.claimed_by.as_deref().and_then(id_to_worker(&runs[0].0));
            prop_assert_eq!(owner, m.owner, "owner divergence on r{}", i);
            prop_assert!(
                store.get_run(&wrong(i), r).await.unwrap().is_none(),
                "run r{} is visible to the wrong tenant",
                i
            );
            if m.status == RunStatus::Running {
                prop_assert!(m.owner.is_some(), "running r{} has no owner", i);
            }
        }
    }
    Ok(())
}

fn id_to_worker(prefix: &str) -> impl Fn(&str) -> Option<usize> + '_ {
    move |claimed: &str| {
        claimed
            .strip_prefix(prefix)?
            .strip_prefix('w')?
            .parse::<usize>()
            .ok()
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, max_shrink_iters: 8000, ..ProptestConfig::default() })]

    #[test]
    fn memory_run_state_machine_matches_the_model(ops in prop::collection::vec(op(), 1..24)) {
        let runs = runs_for("");
        Runtime::new().unwrap().block_on(run_model(&MemorySessionStore::new(), &runs, true, ops))?;
    }
}

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use runic_substrate::PostgresSessionStore;
    use sqlx::postgres::PgPoolOptions;

    static CASE: AtomicU64 = AtomicU64::new(0);

    async fn store() -> Option<PostgresSessionStore> {
        let url = std::env::var("RUNIC_TEST_DATABASE_URL").ok()?;
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("RUNIC_TEST_DATABASE_URL unreachable");
        Some(PostgresSessionStore::from_pool(pool).await.unwrap())
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, max_shrink_iters: 2000, ..ProptestConfig::default() })]

        #[test]
        fn postgres_run_state_machine_matches_the_model(ops in prop::collection::vec(op(), 1..18)) {
            let rt = Runtime::new().unwrap();
            rt.block_on(async {
                let Some(store) = store().await else { return Ok(()); };
                let prefix = format!("plt-{}-", CASE.fetch_add(1, Ordering::SeqCst));
                let runs = runs_for(&prefix);
                run_model(&store, &runs, false, ops).await
            })?;
        }
    }
}
