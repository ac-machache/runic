use std::collections::HashMap;

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

fn terminal(s: RunStatus) -> bool {
    matches!(
        s,
        RunStatus::Successful | RunStatus::Failed | RunStatus::Cancelled
    )
}

#[derive(Debug, Clone)]
enum Op {
    TryStart(usize),
    Complete(usize),
    Fail(usize),
    Wait(usize),
    Resume(usize),
    Cancel(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0usize..4).prop_map(Op::TryStart),
        (0usize..4).prop_map(Op::Complete),
        (0usize..4).prop_map(Op::Fail),
        (0usize..4).prop_map(Op::Wait),
        (0usize..4).prop_map(Op::Resume),
        (0usize..4).prop_map(Op::Cancel),
    ]
}

async fn run_model(
    store: &dyn SessionStore,
    runs: &[(String, String); 4],
    ops: Vec<Op>,
) -> Result<(), TestCaseError> {
    let t0 = runs[0].1.clone();
    let t1 = runs[2].1.clone();
    let wrong = |i: usize| {
        if runs[i].1 == t0 {
            t1.clone()
        } else {
            t0.clone()
        }
    };

    let mut model: HashMap<usize, RunStatus> = HashMap::new();
    for (i, (r, t)) in runs.iter().enumerate() {
        store.create_run(t, "s", r, "coral").await.unwrap();
        model.insert(i, RunStatus::Idle);
    }

    for op in ops {
        match op {
            Op::TryStart(ri) => {
                let status = model[&ri];
                let thread_running = model.iter().any(|(&i, &st)| {
                    i != ri && runs[i].1 == runs[ri].1 && st == RunStatus::Running
                });
                let older_waiting = model
                    .iter()
                    .any(|(&i, &st)| i < ri && runs[i].1 == runs[ri].1 && st == RunStatus::Idle);
                let startable = matches!(status, RunStatus::Idle | RunStatus::Waiting);
                let expect = startable && !thread_running && !older_waiting;
                let got = store.try_start_run(&runs[ri].1, &runs[ri].0).await.unwrap();
                prop_assert_eq!(got, expect, "try_start_run r{}", ri);
                if got {
                    model.insert(ri, RunStatus::Running);
                }
            }
            Op::Complete(ri) => {
                store
                    .set_run_status(&runs[ri].0, RunStatus::Successful, None)
                    .await
                    .unwrap();
                model.insert(ri, RunStatus::Successful);
            }
            Op::Fail(ri) => {
                store
                    .set_run_status(&runs[ri].0, RunStatus::Failed, Some("boom"))
                    .await
                    .unwrap();
                model.insert(ri, RunStatus::Failed);
            }
            Op::Wait(ri) => {
                store
                    .set_run_status(&runs[ri].0, RunStatus::Waiting, None)
                    .await
                    .unwrap();
                model.insert(ri, RunStatus::Waiting);
            }
            Op::Resume(ri) => {
                let expect = model[&ri] == RunStatus::Waiting;
                let got = store.resume_run(&runs[ri].1, &runs[ri].0).await.unwrap();
                prop_assert_eq!(got, expect, "resume r{}", ri);
                if expect {
                    model.insert(ri, RunStatus::Idle);
                }
            }
            Op::Cancel(ri) => {
                let status = model[&ri];
                let got = store
                    .request_cancel_run(&runs[ri].1, &runs[ri].0)
                    .await
                    .unwrap();
                if terminal(status) {
                    prop_assert!(!got, "cancel of terminal r{} returned true", ri);
                } else {
                    prop_assert!(got, "cancel of non-terminal r{} returned false", ri);
                    if matches!(status, RunStatus::Idle | RunStatus::Waiting) {
                        model.insert(ri, RunStatus::Cancelled);
                    }
                }
            }
        }

        for (i, (r, t)) in runs.iter().enumerate() {
            let rec = store.get_run(t, r).await.unwrap().unwrap();
            let want = model[&i];
            prop_assert_eq!(rec.status, want, "status divergence on r{}", i);
            prop_assert!(
                store.get_run(&wrong(i), r).await.unwrap().is_none(),
                "run r{} is visible to the wrong tenant",
                i
            );
        }

        for tenant in [&t0, &t1] {
            let running = model
                .iter()
                .filter(|&(&i, &st)| runs[i].1 == *tenant && st == RunStatus::Running)
                .count();
            prop_assert!(
                running <= 1,
                "thread {tenant} has more than one running run"
            );
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, max_shrink_iters: 8000, ..ProptestConfig::default() })]

    #[test]
    fn memory_run_state_machine_matches_the_model(ops in prop::collection::vec(op(), 1..24)) {
        let runs = runs_for("");
        Runtime::new().unwrap().block_on(run_model(&MemorySessionStore::new(), &runs, ops))?;
    }
}

#[cfg(feature = "postgres")]
mod pg {
    use super::*;

    use runic_substrate::PostgresSessionStore;
    use sqlx::postgres::PgPoolOptions;

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
                let prefix = format!("plt-{}-", uuid::Uuid::new_v4().simple());
                let runs = runs_for(&prefix);
                run_model(&store, &runs, ops).await
            })?;
        }
    }
}
