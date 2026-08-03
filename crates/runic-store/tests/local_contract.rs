mod common;

use std::path::{Path, PathBuf};

use runic_store::artifacts::{self, ArtifactFiles};
use runic_store::{ArtifactSource, ArtifactStore, Error};

use crate::common::ids::uid;

fn fresh_root() -> PathBuf {
    // Intentionally not created here — `put` must create it lazily.
    std::env::temp_dir().join(uid("runic-substrate-local"))
}

fn store_at(root: impl AsRef<Path>) -> ArtifactFiles {
    artifacts::local(root.as_ref().to_string_lossy()).unwrap()
}

artifact_store_contract_suite!(|| async { Some(store_at(fresh_root())) });
artifact_store_delete_from_list_suite!(|| async { Some(store_at(fresh_root())) });
artifact_store_stress_suite!(|| async { Some(store_at(fresh_root())) });

async fn put_text(store: &ArtifactFiles, t: &str, s: &str, body: &[u8]) -> String {
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
    let store = store_at(&root);
    let id = put_text(&store, "t", "s", b"x").await;
    assert!(root.join("blobs").join(&id).exists());
}

#[tokio::test]
async fn corrupt_metadata_file_is_error_not_panic() {
    let root = fresh_root();
    let store = store_at(&root);
    let id = put_text(&store, "t", "s", b"x").await;
    tokio::fs::write(
        root.join("blobs").join(format!("{id}.json")),
        b"{not valid json",
    )
    .await
    .unwrap();
    // head must surface a typed error, never panic
    assert!(matches!(store.head(&id).await, Err(Error::Serde(_))));
    // bytes are independent of the corrupt metadata
    assert_eq!(store.get(&id).await.unwrap(), b"x");
}

#[tokio::test]
async fn missing_blob_with_existing_metadata() {
    let root = fresh_root();
    let store = store_at(&root);
    let id = put_text(&store, "t", "s", b"x").await;
    tokio::fs::remove_file(root.join("blobs").join(&id))
        .await
        .unwrap();
    assert!(matches!(store.get(&id).await, Err(Error::NotFound(_))));
    assert!(store.head(&id).await.is_ok(), "metadata still present");
}

#[tokio::test]
async fn existing_blob_with_missing_metadata() {
    let root = fresh_root();
    let store = store_at(&root);
    let id = put_text(&store, "t", "s", b"x").await;
    tokio::fs::remove_file(root.join("blobs").join(format!("{id}.json")))
        .await
        .unwrap();
    assert!(matches!(store.head(&id).await, Err(Error::NotFound(_))));
    assert_eq!(store.get(&id).await.unwrap(), b"x", "bytes still present");
}

#[tokio::test]
async fn unreadable_index_entry_is_skipped() {
    let root = fresh_root();
    let store = store_at(&root);
    put_text(&store, "t", "s", b"good").await;

    let marker = root.join("index").join("t").join("s").join("art-ghost");
    tokio::fs::write(&marker, b"").await.unwrap();

    assert_eq!(
        store.list("t", "s").await.unwrap().len(),
        1,
        "valid entries survive an index entry with no metadata behind it"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn permission_denied_maps_to_io_error() {
    use std::os::unix::fs::PermissionsExt;
    let root = fresh_root();
    let blobs = root.join("blobs");
    tokio::fs::create_dir_all(&blobs).await.unwrap();
    tokio::fs::set_permissions(&blobs, std::fs::Permissions::from_mode(0o555))
        .await
        .unwrap();

    let r = store_at(&root)
        .put("t", "s", "text/plain", ArtifactSource::UserUpload, b"x")
        .await;
    tokio::fs::set_permissions(&blobs, std::fs::Permissions::from_mode(0o755))
        .await
        .unwrap();

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
    let store = store_at(fresh_root());
    store.delete(&uid("art-never")).await.unwrap();
}

#[tokio::test]
async fn artifact_id_whitelist_rejects_path_tricks() {
    let root = fresh_root();
    // a file the attacker would love to reach, just outside the store root
    let outside = root.parent().unwrap().join(uid("secret"));
    tokio::fs::create_dir_all(root.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&outside, b"do not touch").await.unwrap();
    let store = store_at(&root);

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
        tokio::fs::read(&outside).await.unwrap(),
        b"do not touch",
        "store wrote/deleted outside its root"
    );
}

#[tokio::test]
async fn delete_clears_the_index_entry() {
    let root = fresh_root();
    let store = store_at(&root);
    let deleted = put_text(&store, "t", "s", b"bye").await;
    let kept = put_text(&store, "t", "s", b"keep").await;

    store.delete(&deleted).await.unwrap();

    let ids: Vec<String> = store
        .list("t", "s")
        .await
        .unwrap()
        .into_iter()
        .map(|artifact| artifact.id)
        .collect();
    assert_eq!(ids, vec![kept]);

    assert!(
        !root
            .join("index")
            .join("t")
            .join("s")
            .join(&deleted)
            .exists(),
        "the index entry is gone, not merely filtered at read time"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_inside_root_is_followed_trusted_root() {
    let root = fresh_root();
    let blobs = root.join("blobs");
    tokio::fs::create_dir_all(&blobs).await.unwrap();
    let outside = root.join("outside.txt");
    tokio::fs::write(&outside, b"external").await.unwrap();
    tokio::fs::symlink(&outside, blobs.join("linkid"))
        .await
        .unwrap();

    let store = store_at(&root);
    // `linkid` passes the id whitelist and resolves through the symlink.
    assert_eq!(store.get("linkid").await.unwrap(), b"external");
}
