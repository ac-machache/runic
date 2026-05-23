//! Integration tests proving the meta-backends (`OverlayBackend`,
//! `NamespacedBackend`) compose with each other and with concrete backends.

use std::sync::Arc;

use runic_storage_backend::{
    LocalFsBackend, MemoryBackend, NamespacedBackend, OverlayBackend, StorageBackend,
};
use tempfile::tempdir;

#[tokio::test]
async fn overlay_of_namespaced_routes_then_falls_through() {
    // Primary: namespaced backend with two mounts.
    let a = Arc::new(MemoryBackend::new());
    let b = Arc::new(MemoryBackend::new());
    let ns: Arc<dyn StorageBackend> = Arc::new(
        NamespacedBackend::new()
            .mount("a/", a.clone() as Arc<dyn StorageBackend>)
            .mount("b/", b.clone() as Arc<dyn StorageBackend>),
    );
    // Fallback: a plain memory backend.
    let fallback: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
    fallback.write("a/lost", b"in-fallback").await.unwrap();

    let overlay = OverlayBackend::new(vec![ns, fallback]);

    // Routed write into the namespaced primary lands in the right inner mount.
    overlay.write("a/x", b"routed").await.unwrap();
    assert_eq!(a.read("x").await.unwrap(), b"routed");

    // A read that misses the primary (unmounted prefix from the namespaced
    // backend would return NotFound) falls through to the fallback.
    let content = overlay.read("a/lost").await.unwrap();
    assert_eq!(content, b"in-fallback");
}

#[tokio::test]
async fn namespaced_of_overlay_combines_layered_reads_with_routing() {
    // Build two overlays. Each overlay merges its own layers.
    let primary_a = Arc::new(MemoryBackend::new());
    let fallback_a = Arc::new(MemoryBackend::new());
    primary_a.write("hello", b"primary-a").await.unwrap();
    fallback_a.write("only-fallback", b"a-fb").await.unwrap();
    let overlay_a: Arc<dyn StorageBackend> = Arc::new(OverlayBackend::new(vec![
        primary_a.clone() as Arc<dyn StorageBackend>,
        fallback_a.clone() as Arc<dyn StorageBackend>,
    ]));

    let primary_b = Arc::new(MemoryBackend::new());
    primary_b.write("hello", b"primary-b").await.unwrap();
    let overlay_b: Arc<dyn StorageBackend> = Arc::new(OverlayBackend::new(vec![
        primary_b.clone() as Arc<dyn StorageBackend>
    ]));

    // Namespaced routes to whichever overlay matches the prefix.
    let ns = NamespacedBackend::new()
        .mount("a/", overlay_a)
        .mount("b/", overlay_b);

    assert_eq!(ns.read("a/hello").await.unwrap(), b"primary-a");
    assert_eq!(ns.read("a/only-fallback").await.unwrap(), b"a-fb");
    assert_eq!(ns.read("b/hello").await.unwrap(), b"primary-b");
}

#[tokio::test]
async fn overlay_of_local_and_memory_read_modify_write_pattern() {
    // A common "production" shape: primary = local FS, fallback = a read-only
    // remote (simulated here with a pre-populated memory backend).
    let dir = tempdir().unwrap();
    let primary: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(dir.path().to_path_buf()));
    let mirror = Arc::new(MemoryBackend::new());
    mirror.write("docs/intro.md", b"shared intro").await.unwrap();
    let mirror_as_backend: Arc<dyn StorageBackend> = mirror.clone();

    let overlay = OverlayBackend::new(vec![primary.clone(), mirror_as_backend]);

    // First read: falls through to mirror.
    assert_eq!(
        overlay.read("docs/intro.md").await.unwrap(),
        b"shared intro"
    );

    // Write a local override; subsequent reads see the local copy.
    overlay
        .write("docs/intro.md", b"local override")
        .await
        .unwrap();
    assert_eq!(
        overlay.read("docs/intro.md").await.unwrap(),
        b"local override"
    );

    // Mirror is untouched — confirms write-to-primary semantics.
    assert_eq!(
        mirror.read("docs/intro.md").await.unwrap(),
        b"shared intro"
    );
}
