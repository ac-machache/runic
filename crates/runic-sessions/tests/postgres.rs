//! Live round-trip test for `PostgresSessionStore`.
//!
//! Runs only with `--features postgres` AND when `DATABASE_URL` is set;
//! otherwise it skips cleanly so CI without a database stays green.
//!
//!   docker run --rm -d --name runic-pg -p 5432:5432 \
//!     -e POSTGRES_PASSWORD=runic -e POSTGRES_DB=runic postgres:16
//!   DATABASE_URL=postgres://postgres:runic@localhost:5432/runic \
//!     cargo test -p runic-sessions --features postgres --test postgres -- --nocapture

#![cfg(feature = "postgres")]

use chrono::Utc;
use runic_agent_core::SessionEvent;
use runic_sessions::{PostgresSessionStore, SessionStore};

fn ev(run_id: &str) -> SessionEvent {
    SessionEvent::TurnBoundary {
        run_id: run_id.into(),
        at: Utc::now(),
    }
}

#[tokio::test]
async fn postgres_round_trip() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL not set — skipping postgres round-trip");
        return;
    };

    let store = PostgresSessionStore::connect(&url).await.expect("connect");

    let tenant = "test_org";
    let session = "test_session";
    // Idempotent: clear any leftovers from a previous run.
    store.delete_session(tenant, session).await.expect("pre-clean");

    // append → seq is 1-based and monotonic.
    assert_eq!(store.append(tenant, session, &ev("r1")).await.unwrap(), 1);
    assert_eq!(store.append(tenant, session, &ev("r1")).await.unwrap(), 2);
    assert_eq!(store.append(tenant, session, &ev("r1")).await.unwrap(), 3);

    // read → all three, in seq order.
    let all = store.read(tenant, session).await.unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3]);

    // read_after(1) → only seq 2 and 3.
    let tail = store.read_after(tenant, session, 1).await.unwrap();
    assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);

    // list_sessions → our session shows up for this tenant.
    let sessions = store.list_sessions(tenant).await.unwrap();
    assert!(sessions.contains(&session.to_string()));

    // tenant isolation → a different tenant sees nothing.
    let other = store.read("other_org", session).await.unwrap();
    assert!(other.is_empty(), "tenant isolation breached");

    // delete → read comes back empty.
    store.delete_session(tenant, session).await.unwrap();
    assert!(store.read(tenant, session).await.unwrap().is_empty());

    eprintln!("postgres round-trip OK");
}
