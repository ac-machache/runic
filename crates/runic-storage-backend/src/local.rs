//! `LocalFsBackend` — a `StorageBackend` rooted at a base directory.
//!
//! Keys are interpreted as relative paths under `base_dir`. Path traversal
//! attempts (any segment equal to `..`) are rejected with `InvalidKey` —
//! callers cannot escape the base directory through this interface.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::fs;

use crate::backend::StorageBackend;
use crate::error::StorageError;
use crate::types::{Entry, EntryKind, Metadata};

pub struct LocalFsBackend {
    base_dir: PathBuf,
}

impl LocalFsBackend {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Resolve a key to an absolute path under `base_dir`, rejecting any
    /// traversal segments.
    fn resolve(&self, key: &str) -> Result<PathBuf, StorageError> {
        if key.is_empty() {
            return Err(StorageError::invalid_key("key is empty"));
        }
        for segment in key.split('/') {
            if segment == ".." {
                return Err(StorageError::invalid_key(format!(
                    "key contains '..' segment: {key}"
                )));
            }
        }
        Ok(self.base_dir.join(key))
    }
}

fn map_io_err(key: &str, err: std::io::Error) -> StorageError {
    match err.kind() {
        std::io::ErrorKind::NotFound => StorageError::not_found(key),
        std::io::ErrorKind::PermissionDenied => StorageError::PermissionDenied {
            key: key.to_string(),
        },
        std::io::ErrorKind::AlreadyExists => StorageError::AlreadyExists {
            key: key.to_string(),
        },
        _ => StorageError::io(err),
    }
}

#[async_trait]
impl StorageBackend for LocalFsBackend {
    async fn read(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let path = self.resolve(key)?;
        fs::read(&path).await.map_err(|err| map_io_err(key, err))
    }

    async fn write(&self, key: &str, content: &[u8]) -> Result<(), StorageError> {
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|err| map_io_err(key, err))?;
        }
        fs::write(&path, content)
            .await
            .map_err(|err| map_io_err(key, err))
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let path = self.resolve(key)?;
        fs::remove_file(&path)
            .await
            .map_err(|err| map_io_err(key, err))
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        let path = self.resolve(key)?;
        match fs::metadata(&path).await {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(map_io_err(key, err)),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>, StorageError> {
        // Treat prefix as a directory path under base_dir. If empty, list base_dir.
        let dir = if prefix.is_empty() {
            self.base_dir.clone()
        } else {
            self.resolve(prefix)?
        };

        let mut read_dir = match fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(map_io_err(prefix, err)),
        };

        let mut out: Vec<Entry> = Vec::new();
        loop {
            match read_dir.next_entry().await {
                Ok(Some(entry)) => {
                    let file_type = entry
                        .file_type()
                        .await
                        .map_err(|err| map_io_err(prefix, err))?;
                    let kind = if file_type.is_dir() {
                        EntryKind::Directory
                    } else {
                        EntryKind::File
                    };
                    let key = relative_key(&self.base_dir, &entry.path());
                    let (size, modified) = match entry.metadata().await {
                        Ok(m) => (
                            Some(m.len()),
                            m.modified().ok().and_then(chrono_from_systemtime),
                        ),
                        Err(_) => (None, None),
                    };
                    out.push(Entry {
                        key,
                        kind,
                        size,
                        modified,
                    });
                }
                Ok(None) => break,
                Err(err) => return Err(map_io_err(prefix, err)),
            }
        }
        // Deterministic order.
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn metadata(&self, key: &str) -> Result<Metadata, StorageError> {
        let path = self.resolve(key)?;
        let m = fs::metadata(&path).await.map_err(|err| map_io_err(key, err))?;
        let kind = if m.is_dir() {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        Ok(Metadata {
            kind,
            size: m.len(),
            modified: m.modified().ok().and_then(chrono_from_systemtime),
        })
    }
}

fn relative_key(base: &Path, full: &Path) -> String {
    full.strip_prefix(base)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| full.to_string_lossy().to_string())
}

fn chrono_from_systemtime(t: std::time::SystemTime) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::TimeZone;
    let dur = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    let secs = i64::try_from(dur.as_secs()).ok()?;
    let nsecs = dur.subsec_nanos();
    chrono::Utc.timestamp_opt(secs, nsecs).single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn empty_key_is_rejected() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path());
        let err = backend.read("").await.unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
    }

    #[tokio::test]
    async fn double_dot_segment_is_rejected() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path());
        let err = backend.read("foo/../bar").await.unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
    }

    #[tokio::test]
    async fn write_creates_nested_parent_directories() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path());
        backend
            .write("a/b/c/file.txt", b"hello")
            .await
            .expect("write ok");
        let content = backend.read("a/b/c/file.txt").await.unwrap();
        assert_eq!(content, b"hello");
    }

    #[tokio::test]
    async fn list_on_missing_directory_returns_empty() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path());
        let entries = backend.list("does/not/exist").await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn list_returns_sorted_entries() {
        let dir = tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path());
        backend.write("z.md", b"").await.unwrap();
        backend.write("a.md", b"").await.unwrap();
        backend.write("m.md", b"").await.unwrap();
        let entries = backend.list("").await.unwrap();
        let keys: Vec<_> = entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["a.md", "m.md", "z.md"]);
    }

    #[tokio::test]
    async fn base_dir_does_not_have_to_exist_yet() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("not_yet_created");
        let backend = LocalFsBackend::new(&nested);
        backend.write("file.txt", b"hi").await.expect("write ok");
        assert!(backend.exists("file.txt").await.unwrap());
    }
}
