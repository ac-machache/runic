//! `ArtifactStore` contract — guarantees every artifact backend must uphold.
//!
//! Note on scope: `get`/`head`/`delete` are keyed by opaque id alone (no
//! tenant) by design — tenant ownership is enforced one layer up
//! (`ReadThreadArtifactTool`). So isolation here is asserted through `list`,
//! which IS tenant/session scoped.

use chrono::{Duration, Utc};

use runic_substrate::{ArtifactSource, ArtifactStore, Error};

use crate::common::ids::{tenant_session, uid};

async fn put_text(store: &dyn ArtifactStore, t: &str, s: &str, body: &[u8]) -> String {
    store
        .put(t, s, "text/plain", ArtifactSource::UserUpload, body)
        .await
        .unwrap()
        .id
}

pub async fn put_get_roundtrip_exact_bytes(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    let body = b"the quick brown fox";
    let id = put_text(store, &t, &s, body).await;
    assert_eq!(store.get(&id).await.unwrap(), body);
}

pub async fn head_returns_metadata(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    let a = store
        .put(
            &t,
            &s,
            "application/pdf",
            ArtifactSource::UserUpload,
            b"%PDF-1.7",
        )
        .await
        .unwrap();
    let head = store.head(&a.id).await.unwrap();
    assert_eq!(head.id, a.id);
    assert_eq!(head.mime_type, "application/pdf");
    assert_eq!(head.size, 8);
    assert_eq!(head.source, ArtifactSource::UserUpload);
}

pub async fn list_returns_session_artifacts(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    put_text(store, &t, &s, b"one").await;
    put_text(store, &t, &s, b"two").await;
    assert_eq!(store.list(&t, &s).await.unwrap().len(), 2);
}

pub async fn list_is_tenant_session_scoped(store: &dyn ArtifactStore) {
    let (t1, t2) = (uid("tenant"), uid("tenant"));
    let (s1, s2) = (uid("sess"), uid("sess"));
    put_text(store, &t1, &s1, b"a").await;
    put_text(store, &t1, &s2, b"b").await;
    put_text(store, &t2, &s1, b"c").await;
    assert_eq!(store.list(&t1, &s1).await.unwrap().len(), 1);
    assert!(store.list(&t2, &s2).await.unwrap().is_empty());
    // a different tenant with the SAME session id sees nothing of t1's
    assert_eq!(store.list(&t2, &s1).await.unwrap().len(), 1);
}

pub async fn get_after_delete_is_notfound(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    let id = put_text(store, &t, &s, b"bye").await;
    store.delete(&id).await.unwrap();
    assert!(matches!(store.get(&id).await, Err(Error::NotFound(_))));
}

pub async fn head_after_delete_is_notfound(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    let id = put_text(store, &t, &s, b"bye").await;
    store.delete(&id).await.unwrap();
    assert!(matches!(store.head(&id).await, Err(Error::NotFound(_))));
}

/// Stronger guarantee: `list` no longer returns a deleted artifact. All three
/// backends satisfy this (Local skips index entries whose blob is gone).
pub async fn list_after_delete_excludes_artifact(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    let id = put_text(store, &t, &s, b"bye").await;
    put_text(store, &t, &s, b"keep").await;
    store.delete(&id).await.unwrap();
    let listed = store.list(&t, &s).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed.iter().all(|a| a.id != id));
}

pub async fn delete_is_idempotent(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    let id = put_text(store, &t, &s, b"x").await;
    store.delete(&id).await.unwrap();
    store.delete(&id).await.unwrap(); // second delete is a no-op, not an error
    store.delete(&uid("art-never")).await.unwrap(); // unknown id is fine too
}

pub async fn empty_bytes_roundtrip(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    let a = store
        .put(
            &t,
            &s,
            "application/octet-stream",
            ArtifactSource::ToolOutput,
            b"",
        )
        .await
        .unwrap();
    assert_eq!(a.size, 0);
    assert_eq!(store.get(&a.id).await.unwrap(), b"");
}

pub async fn binary_bytes_with_zeros_roundtrip(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    let mut body: Vec<u8> = (0u8..=255).collect();
    body.extend_from_slice(&[0, 0, 0, 255, 254, 0, 1]);
    let a = store
        .put(
            &t,
            &s,
            "application/octet-stream",
            ArtifactSource::ModelOutput,
            &body,
        )
        .await
        .unwrap();
    assert_eq!(store.get(&a.id).await.unwrap(), body);
    assert_eq!(a.size, body.len() as u64);
}

pub async fn artifact_ids_are_unique(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    let mut ids = std::collections::HashSet::new();
    for _ in 0..50 {
        assert!(
            ids.insert(put_text(store, &t, &s, b"x").await),
            "duplicate artifact id"
        );
    }
}

pub async fn many_artifacts_in_session_list(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    for i in 0..60 {
        put_text(store, &t, &s, format!("n{i}").as_bytes()).await;
    }
    assert_eq!(store.list(&t, &s).await.unwrap().len(), 60);
}

pub async fn mime_type_roundtrips(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    for mime in [
        "text/plain; charset=utf-8",
        "image/png",
        "application/vnd.custom+json",
    ] {
        let a = store
            .put(&t, &s, mime, ArtifactSource::Other, b"x")
            .await
            .unwrap();
        assert_eq!(store.head(&a.id).await.unwrap().mime_type, mime);
    }
}

pub async fn source_roundtrips_all_variants(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    for src in [
        ArtifactSource::UserUpload,
        ArtifactSource::ToolOutput,
        ArtifactSource::ModelOutput,
        ArtifactSource::Other,
    ] {
        let a = store.put(&t, &s, "text/plain", src, b"x").await.unwrap();
        assert_eq!(store.head(&a.id).await.unwrap().source, src);
    }
}

pub async fn created_at_is_sane(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    let before = Utc::now() - Duration::seconds(5);
    let a = store
        .put(&t, &s, "text/plain", ArtifactSource::UserUpload, b"x")
        .await
        .unwrap();
    let after = Utc::now() + Duration::seconds(5);
    assert!(
        a.created_at >= before && a.created_at <= after,
        "created_at {} out of range",
        a.created_at
    );
}

pub async fn list_order_is_deterministic(store: &dyn ArtifactStore) {
    let (t, s) = tenant_session();
    for i in 0..5 {
        put_text(store, &t, &s, format!("n{i}").as_bytes()).await;
    }
    let a = store.list(&t, &s).await.unwrap();
    let b = store.list(&t, &s).await.unwrap();
    let ids_a: Vec<_> = a.iter().map(|x| &x.id).collect();
    let ids_b: Vec<_> = b.iter().map(|x| &x.id).collect();
    assert_eq!(ids_a, ids_b, "list order must be stable across calls");
}

/// `tenant/a` vs `tenant_a` and `session/1` vs `session_1` are distinct keys —
/// the classic path-segment collision.
pub async fn weird_tenant_session_no_collision(store: &dyn ArtifactStore) {
    let base = uid("x");
    let cases = [
        (format!("{base}/a"), format!("{base}-s")),
        (format!("{base}_a"), format!("{base}-s")),
        (format!("{base}-t"), format!("{base}/1")),
        (format!("{base}-t"), format!("{base}_1")),
    ];
    let mut ids = Vec::new();
    for (t, s) in &cases {
        ids.push(put_text(store, t, s, t.as_bytes()).await);
    }
    for (i, (t, s)) in cases.iter().enumerate() {
        let listed = store.list(t, s).await.unwrap();
        assert_eq!(listed.len(), 1, "collision at {t}/{s}");
        assert_eq!(listed[0].id, ids[i]);
    }
}

/// A traversal-shaped id resolves to NotFound (never reads outside the store);
/// it must never panic or return foreign bytes.
pub async fn malicious_id_get_is_notfound_not_traversal(store: &dyn ArtifactStore) {
    for evil in [
        "../../etc/passwd",
        "..",
        "/etc/passwd",
        r"..\..\win",
        "%2e%2e%2fpasswd",
    ] {
        assert!(
            matches!(store.get(evil).await, Err(Error::NotFound(_))),
            "get({evil})"
        );
        assert!(
            matches!(store.head(evil).await, Err(Error::NotFound(_))),
            "head({evil})"
        );
        store.delete(evil).await.unwrap(); // delete of a bogus id is a safe no-op
    }
}

/// Weird tenant/session *names* are encoded safely: bytes roundtrip and a
/// neighbouring tenant can't see them.
pub async fn weird_names_do_not_escape_or_cross(store: &dyn ArtifactStore) {
    let t = format!("{}/../escape", uid("tenant"));
    let s = format!("{}/../escape", uid("sess"));
    let id = put_text(store, &t, &s, b"contained").await;
    assert_eq!(store.get(&id).await.unwrap(), b"contained");
    assert_eq!(store.list(&t, &s).await.unwrap().len(), 1);
    assert!(
        store
            .list(&uid("other-tenant"), &s)
            .await
            .unwrap()
            .is_empty()
    );
}
