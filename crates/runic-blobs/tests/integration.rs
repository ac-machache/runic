//! Integration tests for runic-blobs — exercises BlobStore +
//! FileBlobStore + BlobStoreResolver across both in-memory and
//! real-filesystem backends.

use runic_blobs::{
    BlobError, BlobInput, BlobResolver, BlobStore, BlobStoreResolver, FileBlobStore,
};
use runic_storage_backend::{LocalFsBackend, MemoryBackend, StorageBackend};
use std::sync::Arc;
use tempfile::tempdir;

fn fresh_store() -> Arc<dyn BlobStore> {
    let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
    Arc::new(FileBlobStore::new(storage))
}

const SHA256_HEX_LEN: usize = 64;

// ─── put + read round-trips ─────────────────────────────────────────────────

#[tokio::test]
async fn put_then_read_round_trips_bytes() {
    let store = fresh_store();
    let bytes = b"hello world".to_vec();
    let r = store
        .put("alice", BlobInput::new(bytes.clone(), "text/plain"))
        .await
        .unwrap();

    assert_eq!(r.id.len(), SHA256_HEX_LEN);
    assert_eq!(r.mime, "text/plain");
    assert_eq!(r.size, bytes.len() as u64);
    assert!(r.name.is_none());

    let read = store.read("alice", &r.id).await.unwrap();
    assert_eq!(read, bytes);
}

#[tokio::test]
async fn put_preserves_optional_name() {
    let store = fresh_store();
    let r = store
        .put(
            "alice",
            BlobInput::new(b"hello".to_vec(), "text/plain").with_name("hi.txt"),
        )
        .await
        .unwrap();
    assert_eq!(r.name.as_deref(), Some("hi.txt"));

    let meta = store.metadata("alice", &r.id).await.unwrap();
    assert_eq!(meta.name.as_deref(), Some("hi.txt"));
}

// ─── content addressing + dedup ────────────────────────────────────────────

#[tokio::test]
async fn identical_bytes_produce_identical_ids() {
    let store = fresh_store();
    let r1 = store
        .put("alice", BlobInput::new(b"same bytes".to_vec(), "text/plain"))
        .await
        .unwrap();
    let r2 = store
        .put("alice", BlobInput::new(b"same bytes".to_vec(), "text/plain"))
        .await
        .unwrap();
    assert_eq!(r1.id, r2.id, "content-addressed → same bytes → same id");
}

#[tokio::test]
async fn different_bytes_produce_different_ids() {
    let store = fresh_store();
    let r1 = store
        .put("alice", BlobInput::new(b"hello".to_vec(), "text/plain"))
        .await
        .unwrap();
    let r2 = store
        .put("alice", BlobInput::new(b"world".to_vec(), "text/plain"))
        .await
        .unwrap();
    assert_ne!(r1.id, r2.id);
}

#[tokio::test]
async fn identical_bytes_with_different_mime_still_share_id() {
    // Content-addressing IS about bytes only. The MIME type is metadata,
    // not part of the address. If you upload the same bytes once as
    // octet-stream and once as png, the id is identical — the later
    // metadata overwrites earlier metadata.
    let store = fresh_store();
    let r1 = store
        .put(
            "alice",
            BlobInput::new(b"shared".to_vec(), "application/octet-stream"),
        )
        .await
        .unwrap();
    let r2 = store
        .put("alice", BlobInput::new(b"shared".to_vec(), "image/png"))
        .await
        .unwrap();
    assert_eq!(r1.id, r2.id);
    assert_eq!(r2.mime, "image/png");

    let meta = store.metadata("alice", &r1.id).await.unwrap();
    assert_eq!(meta.mime, "image/png", "second put's mime wins");
}

// ─── tenant isolation ──────────────────────────────────────────────────────

#[tokio::test]
async fn tenants_have_isolated_views_even_for_identical_bytes() {
    let store = fresh_store();
    let bytes = b"shared content".to_vec();
    let alice_ref = store
        .put("alice", BlobInput::new(bytes.clone(), "text/plain"))
        .await
        .unwrap();
    let bob_ref = store
        .put("bob", BlobInput::new(bytes.clone(), "text/plain"))
        .await
        .unwrap();

    // Same content hash (it's the same bytes).
    assert_eq!(alice_ref.id, bob_ref.id);

    // But bob can't read alice's blob even with the same id.
    // The id IS the same, AND bob has his own copy — so this actually
    // succeeds for bob's tenant. The test is really: deletion from one
    // tenant doesn't affect the other.
    store.delete("alice", &alice_ref.id).await.unwrap();
    assert!(!store.exists("alice", &alice_ref.id).await.unwrap());
    assert!(store.exists("bob", &bob_ref.id).await.unwrap(), "bob's copy survives");
}

#[tokio::test]
async fn read_for_wrong_tenant_returns_not_found() {
    let store = fresh_store();
    let r = store
        .put("alice", BlobInput::new(b"secret".to_vec(), "text/plain"))
        .await
        .unwrap();
    let err = store.read("bob", &r.id).await.unwrap_err();
    match err {
        BlobError::NotFound { tenant, id } => {
            assert_eq!(tenant, "bob");
            assert_eq!(id, r.id);
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

// ─── metadata ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn metadata_returns_size_mime_and_tenant() {
    let store = fresh_store();
    let r = store
        .put(
            "alice",
            BlobInput::new(b"hello".to_vec(), "text/plain").with_name("greeting.txt"),
        )
        .await
        .unwrap();
    let meta = store.metadata("alice", &r.id).await.unwrap();
    assert_eq!(meta.size, 5);
    assert_eq!(meta.mime, "text/plain");
    assert_eq!(meta.name.as_deref(), Some("greeting.txt"));
    assert_eq!(meta.tenant, "alice");
    assert_eq!(meta.id, r.id);
}

#[tokio::test]
async fn metadata_for_missing_blob_is_not_found() {
    let store = fresh_store();
    let err = store.metadata("alice", "deadbeef").await.unwrap_err();
    assert!(matches!(err, BlobError::NotFound { .. }));
}

// ─── exists / delete / list ─────────────────────────────────────────────────

#[tokio::test]
async fn exists_returns_false_before_put_and_true_after() {
    let store = fresh_store();
    assert!(!store.exists("alice", "doesnt-exist").await.unwrap());
    let r = store
        .put("alice", BlobInput::new(b"x".to_vec(), "text/plain"))
        .await
        .unwrap();
    assert!(store.exists("alice", &r.id).await.unwrap());
}

#[tokio::test]
async fn delete_removes_blob_and_is_idempotent() {
    let store = fresh_store();
    let r = store
        .put("alice", BlobInput::new(b"bye".to_vec(), "text/plain"))
        .await
        .unwrap();
    assert!(store.exists("alice", &r.id).await.unwrap());

    store.delete("alice", &r.id).await.unwrap();
    assert!(!store.exists("alice", &r.id).await.unwrap());

    // Idempotent — second delete is fine.
    store.delete("alice", &r.id).await.unwrap();
    store.delete("alice", "never-existed").await.unwrap();
}

#[tokio::test]
async fn list_returns_all_blobs_for_a_tenant() {
    let store = fresh_store();
    let _r1 = store
        .put("alice", BlobInput::new(b"one".to_vec(), "text/plain"))
        .await
        .unwrap();
    let _r2 = store
        .put("alice", BlobInput::new(b"two".to_vec(), "text/plain"))
        .await
        .unwrap();
    let _r3 = store
        .put("alice", BlobInput::new(b"three".to_vec(), "text/plain"))
        .await
        .unwrap();
    let _bob = store
        .put("bob", BlobInput::new(b"bob's".to_vec(), "text/plain"))
        .await
        .unwrap();

    let alice_list = store.list("alice").await.unwrap();
    assert_eq!(alice_list.len(), 3);
    assert!(alice_list.iter().all(|m| m.tenant == "alice"));

    let bob_list = store.list("bob").await.unwrap();
    assert_eq!(bob_list.len(), 1);
}

#[tokio::test]
async fn list_for_empty_tenant_is_empty() {
    let store = fresh_store();
    let list = store.list("ghost").await.unwrap();
    assert!(list.is_empty());
}

// ─── validation ────────────────────────────────────────────────────────────

#[tokio::test]
async fn put_with_empty_mime_is_rejected() {
    let store = fresh_store();
    let err = store
        .put("alice", BlobInput::new(b"x".to_vec(), ""))
        .await
        .unwrap_err();
    assert!(matches!(err, BlobError::InvalidMime(_)));
}

#[tokio::test]
async fn put_with_empty_tenant_is_rejected() {
    let store = fresh_store();
    let err = store
        .put("", BlobInput::new(b"x".to_vec(), "text/plain"))
        .await
        .unwrap_err();
    assert!(matches!(err, BlobError::Storage(_)));
}

// ─── resolver ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn resolver_forwards_to_store_with_baked_in_tenant() {
    let store = fresh_store();
    let r = store
        .put(
            "alice",
            BlobInput::new(b"resolver-test".to_vec(), "text/plain"),
        )
        .await
        .unwrap();

    let resolver = BlobStoreResolver::new(store.clone(), "alice");
    let bytes = resolver.resolve(&r.id).await.unwrap();
    assert_eq!(bytes, b"resolver-test");

    let meta = resolver.metadata(&r.id).await.unwrap();
    assert_eq!(meta.mime, "text/plain");
}

#[tokio::test]
async fn resolver_isolates_to_its_own_tenant() {
    let store = fresh_store();
    let r = store
        .put("alice", BlobInput::new(b"alice".to_vec(), "text/plain"))
        .await
        .unwrap();
    // A resolver baked for bob can't read alice's blob.
    let resolver = BlobStoreResolver::new(store.clone(), "bob");
    let err = resolver.resolve(&r.id).await.unwrap_err();
    assert!(matches!(err, BlobError::NotFound { .. }));
}

// ─── real filesystem ──────────────────────────────────────────────────────

#[tokio::test]
async fn round_trip_against_real_localfs_backend() {
    let dir = tempdir().unwrap();
    let storage: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(dir.path()));
    let store: Arc<dyn BlobStore> = Arc::new(FileBlobStore::new(storage));

    let payload = vec![0xff_u8; 4096]; // 4KB of all-0xff
    let r = store
        .put(
            "tenant-a",
            BlobInput::new(payload.clone(), "application/octet-stream").with_name("binary.bin"),
        )
        .await
        .unwrap();

    let read = store.read("tenant-a", &r.id).await.unwrap();
    assert_eq!(read, payload);
    let meta = store.metadata("tenant-a", &r.id).await.unwrap();
    assert_eq!(meta.size, 4096);
    assert_eq!(meta.name.as_deref(), Some("binary.bin"));

    let list = store.list("tenant-a").await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, r.id);
}

#[tokio::test]
async fn binary_content_round_trips_exactly() {
    let store = fresh_store();
    // All 256 byte values.
    let bytes: Vec<u8> = (0u8..=255).collect();
    let r = store
        .put(
            "t",
            BlobInput::new(bytes.clone(), "application/octet-stream"),
        )
        .await
        .unwrap();
    let read = store.read("t", &r.id).await.unwrap();
    assert_eq!(read, bytes);
}
