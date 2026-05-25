//! runic-storage-backend — the pluggable storage layer behind everything in
//! runic that touches durable state.
//!
//! One trait (`StorageBackend`) with multiple impls (`LocalFsBackend`,
//! `MemoryBackend`, `OverlayBackend`, `NamespacedBackend`, and future cloud
//! impls). Consumers — context-engine layers, persistence, memory tools,
//! skills loader, spillover — all take `Arc<dyn StorageBackend>` and stay
//! storage-agnostic.
//!
//! Keys are plain `&str` (not `Path`) so the same key works against a local
//! directory, an S3 bucket, an in-memory map, or anything else with a
//! string-addressable namespace.
//!
//! ## Conformance suite
//!
//! Every backend impl runs the SAME ~20 tests via the
//! [`conformance_suite!`] macro defined here. That guarantees behavioural
//! consistency across impls — if a new backend is added, it gets the same
//! battery of tests for free.

pub mod backend;
pub mod error;
pub mod local;
pub mod memory;
pub mod namespaced;
pub mod overlay;
pub mod types;

pub use backend::StorageBackend;
pub use error::StorageError;
pub use local::LocalFsBackend;
pub use memory::MemoryBackend;
pub use namespaced::NamespacedBackend;
pub use overlay::OverlayBackend;
pub use types::{Entry, EntryKind, Metadata};

/// Generate the standard conformance test suite for a `StorageBackend` impl.
///
/// Takes a module name and a closure expression that produces a fresh
/// `(Arc<dyn StorageBackend>, impl Drop)` per test. The `Drop` guard is so
/// fixtures like `tempfile::TempDir` stay alive for the duration of the
/// test and clean up afterward; pass `()` if no cleanup is needed.
///
/// ```ignore
/// conformance_suite!(memory, || {
///     use std::sync::Arc;
///     let b: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
///     (b, ())
/// });
/// ```
#[macro_export]
macro_rules! conformance_suite {
    ($mod_name:ident, $make:expr) => {
        #[cfg(test)]
        mod $mod_name {
            use super::*;
            #[allow(unused_imports)]
            use $crate::{StorageBackend, StorageError};

            fn fresh() -> (
                std::sync::Arc<dyn $crate::StorageBackend>,
                Box<dyn std::any::Any>,
            ) {
                let (b, guard) = ($make)();
                (b, Box::new(guard) as Box<dyn std::any::Any>)
            }

            #[tokio::test]
            async fn read_returns_not_found_for_missing() {
                let (b, _g) = fresh();
                let err = b.read("missing").await.unwrap_err();
                assert!(matches!(err, $crate::StorageError::NotFound { .. }));
            }

            #[tokio::test]
            async fn write_then_read_roundtrip() {
                let (b, _g) = fresh();
                b.write("k", b"hello").await.unwrap();
                assert_eq!(b.read("k").await.unwrap(), b"hello");
            }

            #[tokio::test]
            async fn write_then_read_to_string_utf8() {
                let (b, _g) = fresh();
                b.write("k", "héllo 🌍".as_bytes()).await.unwrap();
                assert_eq!(b.read_to_string("k").await.unwrap(), "héllo 🌍");
            }

            #[tokio::test]
            async fn write_empty_content_is_allowed() {
                let (b, _g) = fresh();
                b.write("empty", b"").await.unwrap();
                assert_eq!(b.read("empty").await.unwrap(), b"");
            }

            #[tokio::test]
            async fn write_binary_content_roundtrip() {
                let (b, _g) = fresh();
                let bytes: Vec<u8> = (0u8..=255).collect();
                b.write("bin", &bytes).await.unwrap();
                assert_eq!(b.read("bin").await.unwrap(), bytes);
            }

            #[tokio::test]
            async fn write_large_content_roundtrip() {
                let (b, _g) = fresh();
                let blob = vec![0xab_u8; 1024 * 1024];
                b.write("big", &blob).await.unwrap();
                let read = b.read("big").await.unwrap();
                assert_eq!(read.len(), blob.len());
                assert!(read.iter().all(|&v| v == 0xab));
            }

            #[tokio::test]
            async fn overwrite_existing_key() {
                let (b, _g) = fresh();
                b.write("k", b"first").await.unwrap();
                b.write("k", b"second").await.unwrap();
                assert_eq!(b.read("k").await.unwrap(), b"second");
            }

            #[tokio::test]
            async fn delete_existing_key() {
                let (b, _g) = fresh();
                b.write("k", b"v").await.unwrap();
                b.delete("k").await.unwrap();
                assert!(!b.exists("k").await.unwrap());
            }

            #[tokio::test]
            async fn delete_missing_key_returns_not_found() {
                let (b, _g) = fresh();
                let err = b.delete("never_existed").await.unwrap_err();
                assert!(matches!(err, $crate::StorageError::NotFound { .. }));
            }

            #[tokio::test]
            async fn exists_true_after_write() {
                let (b, _g) = fresh();
                b.write("k", b"v").await.unwrap();
                assert!(b.exists("k").await.unwrap());
            }

            #[tokio::test]
            async fn exists_false_after_delete() {
                let (b, _g) = fresh();
                b.write("k", b"v").await.unwrap();
                b.delete("k").await.unwrap();
                assert!(!b.exists("k").await.unwrap());
            }

            #[tokio::test]
            async fn exists_false_for_never_written() {
                let (b, _g) = fresh();
                assert!(!b.exists("never").await.unwrap());
            }

            #[tokio::test]
            async fn metadata_returns_size_for_known_content() {
                let (b, _g) = fresh();
                b.write("k", b"123456").await.unwrap();
                let meta = b.metadata("k").await.unwrap();
                assert_eq!(meta.size, 6);
            }

            #[tokio::test]
            async fn metadata_returns_not_found_for_missing() {
                let (b, _g) = fresh();
                let err = b.metadata("nope").await.unwrap_err();
                assert!(matches!(err, $crate::StorageError::NotFound { .. }));
            }

            #[tokio::test]
            async fn list_empty_backend_returns_empty() {
                let (b, _g) = fresh();
                let entries = b.list("").await.unwrap();
                assert!(entries.is_empty());
            }

            #[tokio::test]
            async fn list_returns_entries_matching_prefix() {
                let (b, _g) = fresh();
                b.write("foo/a", b"").await.unwrap();
                b.write("foo/b", b"").await.unwrap();
                b.write("bar/c", b"").await.unwrap();
                let entries = b.list("foo").await.unwrap();
                assert!(entries.iter().any(|e| e.key.contains("a")));
                assert!(entries.iter().any(|e| e.key.contains("b")));
                assert!(!entries.iter().any(|e| e.key.contains("c")));
            }

            #[tokio::test]
            async fn list_excludes_entries_outside_prefix() {
                let (b, _g) = fresh();
                b.write("docs/x", b"").await.unwrap();
                b.write("logs/y", b"").await.unwrap();
                let entries = b.list("docs").await.unwrap();
                for e in &entries {
                    assert!(
                        !e.key.contains("logs"),
                        "expected only docs entries, got {:?}",
                        e.key
                    );
                }
            }

            #[tokio::test]
            async fn list_returns_sorted_entries() {
                let (b, _g) = fresh();
                b.write("z.md", b"").await.unwrap();
                b.write("a.md", b"").await.unwrap();
                b.write("m.md", b"").await.unwrap();
                let entries = b.list("").await.unwrap();
                let keys: Vec<String> = entries.iter().map(|e| e.key.clone()).collect();
                let mut sorted = keys.clone();
                sorted.sort();
                assert_eq!(keys, sorted, "list output must be sorted");
            }

            #[tokio::test]
            async fn concurrent_writes_dont_corrupt() {
                let (b, _g) = fresh();
                let mut handles = Vec::new();
                for i in 0..10 {
                    let b = b.clone();
                    handles.push(tokio::spawn(async move {
                        let key = format!("key_{i}");
                        let val = format!("val_{i}");
                        b.write(&key, val.as_bytes()).await.unwrap();
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
                for i in 0..10 {
                    let key = format!("key_{i}");
                    let val = format!("val_{i}");
                    assert_eq!(b.read(&key).await.unwrap(), val.as_bytes());
                }
            }

            #[tokio::test]
            async fn read_to_string_returns_decode_error_for_invalid_utf8() {
                let (b, _g) = fresh();
                let invalid = vec![0xff, 0xfe, 0xfd];
                b.write("bad", &invalid).await.unwrap();
                let err = b.read_to_string("bad").await.unwrap_err();
                assert!(matches!(err, $crate::StorageError::Decode(_)));
            }

            #[tokio::test]
            async fn append_to_missing_key_creates_with_content() {
                let (b, _g) = fresh();
                b.append("fresh", b"hello").await.unwrap();
                assert_eq!(b.read("fresh").await.unwrap(), b"hello");
            }

            #[tokio::test]
            async fn append_extends_existing_key() {
                let (b, _g) = fresh();
                b.write("log", b"line1\n").await.unwrap();
                b.append("log", b"line2\n").await.unwrap();
                b.append("log", b"line3\n").await.unwrap();
                assert_eq!(
                    b.read_to_string("log").await.unwrap(),
                    "line1\nline2\nline3\n"
                );
            }

            #[tokio::test]
            async fn append_empty_content_is_a_noop() {
                let (b, _g) = fresh();
                b.write("k", b"before").await.unwrap();
                b.append("k", b"").await.unwrap();
                assert_eq!(b.read("k").await.unwrap(), b"before");
            }
        }
    };
}

// ─── Conformance suites for the four built-in backends ───────────────────────
// Each backend gets the full 20-test suite via the macro.

#[cfg(test)]
mod conformance_memory {
    use super::*;

    conformance_suite!(memory, || {
        use std::sync::Arc;
        let b: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        (b, ())
    });
}

#[cfg(test)]
mod conformance_local {
    use super::*;

    conformance_suite!(local, || {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let b: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(dir.path().to_path_buf()));
        (b, dir)
    });
}

#[cfg(test)]
mod conformance_overlay {
    use super::*;

    conformance_suite!(overlay, || {
        use std::sync::Arc;
        let primary: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let fallback: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let b: Arc<dyn StorageBackend> = Arc::new(OverlayBackend::new(vec![primary, fallback]));
        (b, ())
    });
}

#[cfg(test)]
mod conformance_namespaced {
    use super::*;

    // For NamespacedBackend the conformance suite operates inside a single
    // mounted namespace. All conformance test keys are short and don't
    // collide with the mount prefix, so we mount the empty prefix to make
    // routing transparent.
    conformance_suite!(namespaced, || {
        use std::sync::Arc;
        let inner: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let b: Arc<dyn StorageBackend> = Arc::new(NamespacedBackend::new().mount("", inner));
        (b, ())
    });
}
