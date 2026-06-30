//! Local (filesystem) artifact backend: the full `ArtifactStore` contract plus
//! path-safety and corruption-handling risks unique to a filesystem store.
//!
//! Each suite store gets a fresh, unique root that does NOT pre-exist, so the
//! whole contract also exercises lazy directory creation.

mod common;

use std::path::PathBuf;

use runic_substrate::{ArtifactSource, ArtifactStore, Error, LocalArtifactStore};

use crate::common::ids::uid;

fn fresh_root() -> PathBuf {
    // Intentionally not created here — `put` must create it lazily.
    std::env::temp_dir().join(uid("runic-substrate-local"))
}

artifact_store_contract_suite!(|| async { Some(LocalArtifactStore::new(fresh_root())) });
artifact_store_delete_from_list_suite!(|| async { Some(LocalArtifactStore::new(fresh_root())) });
artifact_store_stress_suite!(|| async { Some(LocalArtifactStore::new(fresh_root())) });

async fn put_text(store: &LocalArtifactStore, t: &str, s: &str, body: &[u8]) -> String {
    store
        .put(t, s, "text/plain", ArtifactSource::UserUpload, body)
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn root_and_parents_created_lazily() {
    let root = fresh_root().join("deeply").join("nested");
    assert!(!root.exists());
    let store = LocalArtifactStore::new(&root);
    let id = put_text(&store, "t", "s", b"x").await;
    assert!(root.join("blobs").join(&id).exists());
}

#[tokio::test]
async fn corrupt_metadata_file_is_error_not_panic() {
    let root = fresh_root();
    let store = LocalArtifactStore::new(&root);
    let id = put_text(&store, "t", "s", b"x").await;
    std::fs::write(
        root.join("blobs").join(format!("{id}.json")),
        b"{not valid json",
    )
    .unwrap();
    // head must surface a typed error, never panic
    assert!(matches!(store.head(&id).await, Err(Error::Serde(_))));
    // bytes are independent of the corrupt metadata
    assert_eq!(store.get(&id).await.unwrap(), b"x");
}

#[tokio::test]
async fn missing_blob_with_existing_metadata() {
    let root = fresh_root();
    let store = LocalArtifactStore::new(&root);
    let id = put_text(&store, "t", "s", b"x").await;
    std::fs::remove_file(root.join("blobs").join(&id)).unwrap();
    assert!(matches!(store.get(&id).await, Err(Error::NotFound(_))));
    assert!(store.head(&id).await.is_ok(), "metadata still present");
}

#[tokio::test]
async fn existing_blob_with_missing_metadata() {
    let root = fresh_root();
    let store = LocalArtifactStore::new(&root);
    let id = put_text(&store, "t", "s", b"x").await;
    std::fs::remove_file(root.join("blobs").join(format!("{id}.json"))).unwrap();
    assert!(matches!(store.head(&id).await, Err(Error::NotFound(_))));
    assert_eq!(store.get(&id).await.unwrap(), b"x", "bytes still present");
}

#[tokio::test]
async fn corrupt_jsonl_index_line_is_skipped() {
    let root = fresh_root();
    let store = LocalArtifactStore::new(&root);
    put_text(&store, "t", "s", b"good").await;
    // Append a junk line directly to the index — list must skip it, not fail.
    let index = root.join("index").join("t").join("s.jsonl");
    let mut content = std::fs::read_to_string(&index).unwrap();
    content.push_str("this is not json\n");
    std::fs::write(&index, content).unwrap();
    assert_eq!(
        store.list("t", "s").await.unwrap().len(),
        1,
        "valid entries survive a corrupt line"
    );
}

/// A write into a non-writable store dir surfaces as `Error::Io`, not a panic.
/// (Skipped when running as root, which ignores the mode bits.)
#[cfg(unix)]
#[tokio::test]
async fn permission_denied_maps_to_io_error() {
    use std::os::unix::fs::PermissionsExt;
    let root = fresh_root();
    let blobs = root.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    std::fs::set_permissions(&blobs, std::fs::Permissions::from_mode(0o555)).unwrap();

    let r = LocalArtifactStore::new(&root)
        .put("t", "s", "text/plain", ArtifactSource::UserUpload, b"x")
        .await;
    std::fs::set_permissions(&blobs, std::fs::Permissions::from_mode(0o755)).unwrap();

    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    if unsafe { geteuid() } != 0 {
        assert!(
            matches!(r, Err(Error::Io(_))),
            "expected Io error, got {r:?}"
        );
    }
}

#[tokio::test]
async fn delete_of_missing_file_succeeds() {
    let store = LocalArtifactStore::new(fresh_root());
    store.delete(&uid("art-never")).await.unwrap();
}

#[tokio::test]
async fn artifact_id_whitelist_rejects_path_tricks() {
    let root = fresh_root();
    // a file the attacker would love to reach, just outside the store root
    let outside = root.parent().unwrap().join(uid("secret"));
    std::fs::create_dir_all(root.parent().unwrap()).unwrap();
    std::fs::write(&outside, b"do not touch").unwrap();
    let store = LocalArtifactStore::new(&root);

    for evil in [
        "../secret",
        "..",
        "/etc/passwd",
        r"a\b",
        "a/b",
        "%2e%2e",
        ".",
        "  ",
    ] {
        assert!(
            matches!(store.get(evil).await, Err(Error::NotFound(_))),
            "get({evil:?})"
        );
        assert!(
            matches!(store.head(evil).await, Err(Error::NotFound(_))),
            "head({evil:?})"
        );
        store.delete(evil).await.unwrap();
    }
    assert_eq!(
        std::fs::read(&outside).unwrap(),
        b"do not touch",
        "store wrote/deleted outside its root"
    );
}

/// `list` skips entries whose blob is gone — so a deleted artifact (whose
/// append-only index line survives) no longer shows up, and an externally
/// removed blob self-heals out of the listing too.
#[tokio::test]
async fn list_skips_entries_with_missing_blob() {
    let root = fresh_root();
    let store = LocalArtifactStore::new(&root);
    let deleted = put_text(&store, "t", "s", b"bye").await;
    let orphaned = put_text(&store, "t", "s", b"orphan").await;
    let kept = put_text(&store, "t", "s", b"keep").await;

    store.delete(&deleted).await.unwrap();
    std::fs::remove_file(root.join("blobs").join(&orphaned)).unwrap();

    let ids: Vec<String> = store
        .list("t", "s")
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert_eq!(ids, vec![kept]);
}

/// Documents the trusted-root assumption: Local follows a symlink placed inside
/// its own `blobs/` dir. The store does not defend against an attacker who can
/// already write files into the store root — that is out of its threat model.
#[cfg(unix)]
#[tokio::test]
async fn symlink_inside_root_is_followed_trusted_root() {
    let root = fresh_root();
    let blobs = root.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    let outside = root.join("outside.txt");
    std::fs::write(&outside, b"external").unwrap();
    std::os::unix::fs::symlink(&outside, blobs.join("linkid")).unwrap();

    let store = LocalArtifactStore::new(&root);
    // `linkid` passes the id whitelist and resolves through the symlink.
    assert_eq!(store.get("linkid").await.unwrap(), b"external");
}
