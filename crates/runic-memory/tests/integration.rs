//! Integration tests against the real `LocalFsBackend` so we exercise the
//! actual filesystem path the REPL uses.

use std::sync::Arc;

use runic_filesystem::{FilesystemBackend, LocalFs};
use runic_memory::{BoundedMemoryStore, MemoryTool, Target};
use runic_tool::{Tool, ToolContext};
use tempfile::tempdir;

fn ctx() -> ToolContext {
    ToolContext::new("user-1", "session-1", "run-1")
}

#[tokio::test]
async fn writes_land_under_memory_subdir_on_disk() {
    let dir = tempdir().unwrap();
    let backend: Arc<dyn FilesystemBackend> = Arc::new(LocalFs::new(dir.path()));
    let store = BoundedMemoryStore::new(backend);

    store.add(Target::User, "user prefers Rust").await.unwrap();

    let path = dir.path().join("memory").join("USER.md");
    let raw = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(raw, "user prefers Rust");
}

#[tokio::test]
async fn second_entry_uses_section_sign_delimiter() {
    let dir = tempdir().unwrap();
    let backend: Arc<dyn FilesystemBackend> = Arc::new(LocalFs::new(dir.path()));
    let store = BoundedMemoryStore::new(backend);

    store.add(Target::Memory, "first").await.unwrap();
    store.add(Target::Memory, "second").await.unwrap();

    let raw = tokio::fs::read_to_string(dir.path().join("memory/MEMORY.md"))
        .await
        .unwrap();
    assert!(raw.contains("\n§\n"));
    assert!(raw.contains("first"));
    assert!(raw.contains("second"));
}

#[tokio::test]
async fn tool_writes_show_up_when_a_separate_reader_reads() {
    // Models the REPL flow: the tool writes through one Arc<store>,
    // a separate read path picks up the changes.
    let dir = tempdir().unwrap();
    let backend: Arc<dyn FilesystemBackend> = Arc::new(LocalFs::new(dir.path()));
    let store = Arc::new(BoundedMemoryStore::new(backend.clone()));
    let tool = MemoryTool::new(store.clone());

    let result = tool
        .execute(
            serde_json::json!({"action": "add", "target": "memory", "content": "shell is zsh"}),
            &ctx(),
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);

    let from_fs = tokio::fs::read_to_string(dir.path().join("memory/MEMORY.md"))
        .await
        .unwrap();
    assert!(from_fs.contains("shell is zsh"));
}

#[tokio::test]
async fn concurrent_adds_dont_lose_writes() {
    // Hammer the same store from many tasks at once; every successful
    // add must show up in the final read.
    let dir = tempdir().unwrap();
    let backend: Arc<dyn FilesystemBackend> = Arc::new(LocalFs::new(dir.path()));
    let store = Arc::new(BoundedMemoryStore::new(backend).with_limits(100_000, 100_000));

    let mut joins = Vec::new();
    for i in 0..20 {
        let s = store.clone();
        joins.push(tokio::spawn(async move {
            s.add(Target::Memory, &format!("entry {i}")).await
        }));
    }
    for j in joins {
        j.await.unwrap().unwrap();
    }
    let entries = store.read(Target::Memory).await.unwrap();
    assert_eq!(entries.len(), 20);
}
