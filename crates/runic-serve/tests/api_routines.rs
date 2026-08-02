mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use runic_serve::routines::{Routine, RoutineContext};
use runic_serve::store::{ScheduleSpec, Schedules, next_after};

#[derive(Clone, Default)]
struct Counter(Arc<AtomicUsize>);

#[async_trait]
impl Routine for Counter {
    async fn run(&self, _ctx: &RoutineContext) -> anyhow::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Boom;

#[async_trait]
impl Routine for Boom {
    async fn run(&self, _ctx: &RoutineContext) -> anyhow::Result<()> {
        anyhow::bail!("this routine always fails")
    }
}

async fn fired(counter: &Counter, at_least: usize) -> bool {
    for _ in 0..60 {
        if counter.0.load(Ordering::SeqCst) >= at_least {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn settled(schedules: &Schedules, id: &str) -> Option<runic_serve::store::ScheduleRecord> {
    for _ in 0..60 {
        if let Ok(Some(record)) = schedules.get(id).await
            && record.last_fired_at.is_some()
        {
            return Some(record);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

#[tokio::test]
async fn a_due_schedule_fires_its_routine_and_rearms() {
    let Some(h) = common::harness().await else {
        return;
    };
    let counter = Counter::default();
    let _app = runic_serve::router(
        h.config()
            .agent("main", common::agent(Arc::new(common::PanicProvider)))
            .routine("tick", "* * * * * *", counter.clone()),
    );

    let schedules = h.schedules();
    runic_serve::store::Schedules::declare(&schedules, "tick", "* * * * * *", "UTC")
        .await
        .unwrap();

    assert!(fired(&counter, 1).await, "the routine never ran");

    let record = settled(&schedules, "tick").await.expect("it settled");
    assert!(record.last_error.is_none(), "got {:?}", record.last_error);
    assert!(
        record.next_at > Utc::now() - chrono::Duration::seconds(5),
        "next_at was not advanced: {}",
        record.next_at
    );
}

#[tokio::test]
async fn a_failing_routine_records_the_error_and_still_rearms() {
    let Some(h) = common::harness().await else {
        return;
    };
    let _app = runic_serve::router(
        h.config()
            .agent("main", common::agent(Arc::new(common::PanicProvider)))
            .routine("boom", "* * * * * *", Boom),
    );

    let schedules = h.schedules();
    let record = settled(&schedules, "boom").await.expect("it settled");
    assert!(
        record
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("always fails")),
        "got {:?}",
        record.last_error
    );
    assert!(record.enabled, "a failure must not disable the schedule");
}

#[tokio::test]
async fn an_unregistered_routine_is_recorded_rather_than_spun_on() {
    let Some(h) = common::harness().await else {
        return;
    };
    let _app = runic_serve::router(
        h.config()
            .agent("main", common::agent(Arc::new(common::PanicProvider))),
    );

    let schedules = h.schedules();
    schedules
        .create(&ScheduleSpec::new("ghost", "not-registered", "* * * * * *"))
        .await
        .unwrap();

    let record = settled(&schedules, "ghost").await.expect("it settled");
    assert!(
        record
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("not-registered")),
        "got {:?}",
        record.last_error
    );
    assert!(
        record.next_at > Utc::now() - chrono::Duration::seconds(5),
        "an unknown routine must still advance, or it re-claims forever"
    );
}

#[tokio::test]
async fn declaring_a_routine_twice_keeps_its_place_in_the_schedule() {
    let Some(h) = common::harness().await else {
        return;
    };
    let schedules = h.schedules();

    schedules
        .declare("nightly", "0 3 * * *", "UTC")
        .await
        .unwrap();
    let first = schedules.get("nightly").await.unwrap().expect("declared");

    schedules
        .declare("nightly", "0 3 * * *", "UTC")
        .await
        .unwrap();
    let again = schedules.get("nightly").await.unwrap().expect("declared");
    assert_eq!(
        first.next_at, again.next_at,
        "an unchanged declaration must not re-arm on every restart"
    );

    schedules
        .declare("nightly", "0 4 * * *", "UTC")
        .await
        .unwrap();
    let moved = schedules.get("nightly").await.unwrap().expect("declared");
    assert_ne!(
        first.next_at, moved.next_at,
        "changing the cron must move the next occurrence"
    );
    assert_eq!(
        moved.next_at,
        next_after("0 4 * * *", "UTC", Utc::now()).unwrap()
    );
}

#[tokio::test]
async fn a_routine_dropped_from_the_code_is_retired() {
    let Some(h) = common::harness().await else {
        return;
    };
    let schedules = h.schedules();

    schedules.declare("gone", "0 3 * * *", "UTC").await.unwrap();
    schedules.declare("kept", "0 4 * * *", "UTC").await.unwrap();
    schedules
        .create(&ScheduleSpec::new("tenant-owned", "kept", "0 5 * * *"))
        .await
        .unwrap();

    let retired = schedules
        .retire_undeclared(&["kept".to_string()])
        .await
        .unwrap();
    assert_eq!(retired, vec!["gone".to_string()]);

    assert!(schedules.get("kept").await.unwrap().unwrap().enabled);
    assert!(!schedules.get("gone").await.unwrap().unwrap().enabled);
    assert!(
        schedules
            .get("tenant-owned")
            .await
            .unwrap()
            .unwrap()
            .enabled,
        "retiring code-declared schedules must not touch API-created ones"
    );
}
