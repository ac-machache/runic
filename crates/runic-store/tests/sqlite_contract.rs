#![cfg(feature = "sqlite")]

mod common;

use runic_store::{SessionEvent, SessionStore, SqliteSessionStore};
use runic_types::Message;

use crate::common::ids::uid;

async fn store() -> SqliteSessionStore {
    SqliteSessionStore::memory().await.unwrap()
}

fn scratch_db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{}.db", uid("runic-sqlite")))
}

session_store_contract_suite!(|| async { Some(store().await) });
session_store_search_suite!(|| async { Some(store().await) });
session_store_stress_suite!(|| async { Some(store().await) });

fn msg(text: &str) -> SessionEvent {
    SessionEvent::Message {
        run_id: "r".into(),
        msg: Message::user(text),
        at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn a_reopened_database_still_has_its_events() {
    let path = scratch_db();
    let tenant = uid("t");

    let opened = SqliteSessionStore::open(&path).await.unwrap();
    opened
        .append(&tenant, "s", &msg("remember me"))
        .await
        .unwrap();
    drop(opened);

    let reopened = SqliteSessionStore::open(&path).await.unwrap();
    let events = reopened.read(&tenant, "s").await.unwrap();
    assert_eq!(events.len(), 1, "the log survived the close");
    assert_eq!(events[0].seq, 1);

    let hits = reopened
        .search(&tenant, "remember", 10, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "the FTS index survived too");

    tokio::fs::remove_file(&path).await.ok();
}

#[tokio::test]
async fn punctuation_heavy_queries_do_not_blow_up_the_matcher() {
    let store = store().await;
    let tenant = uid("t");
    store
        .append(&tenant, "s", &msg("deploy the service"))
        .await
        .unwrap();

    for query in [
        "",
        "   ",
        "\"",
        "AND",
        "*",
        "a OR b",
        "NEAR(x y)",
        "-",
        "^foo",
    ] {
        let hits = store.search(&tenant, query, 10, None).await;
        assert!(hits.is_ok(), "search({query:?}) errored: {hits:?}");
    }

    assert_eq!(
        store
            .search(&tenant, "deploy", 10, None)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn deleting_a_session_clears_its_search_rows() {
    let store = store().await;
    let tenant = uid("t");
    store
        .append(&tenant, "gone", &msg("findable text"))
        .await
        .unwrap();
    store
        .append(&tenant, "kept", &msg("findable text"))
        .await
        .unwrap();

    store.delete_session(&tenant, "gone").await.unwrap();

    let hits = store.search(&tenant, "findable", 10, None).await.unwrap();
    let sessions: Vec<&str> = hits.iter().map(|hit| hit.session_id.as_str()).collect();
    assert_eq!(
        sessions,
        vec!["kept"],
        "the FTS table has no cascading FK, so delete_session must clear it by hand"
    );
}
