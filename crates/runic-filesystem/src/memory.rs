//! `MemoryFs` — an in-RAM [`FilesystemBackend`] backed by a `BTreeMap`.
//!
//! The ephemeral counterpart to [`LocalFs`](crate::LocalFs): zero-config, no
//! disk, same semantics (create-only `write`, line-sliced `read`). Used for
//! tests and for any backend that should not survive the process — e.g. a
//! scratch mount in a `CompositeBackend`, or curated memory in a dev server.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::{FileInfo, FilesystemBackend, FsError, GrepMatch, ReadResult};

/// An in-memory filesystem. Cheap to construct; share via `Arc`.
#[derive(Default)]
pub struct MemoryFs {
    files: Mutex<BTreeMap<String, String>>,
}

impl MemoryFs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build one pre-seeded with `(path, content)` entries.
    pub fn seeded(entries: &[(&str, &str)]) -> Self {
        let mut m = BTreeMap::new();
        for (k, v) in entries {
            m.insert((*k).to_string(), (*v).to_string());
        }
        Self { files: Mutex::new(m) }
    }

    fn under(base: &str, key: &str) -> bool {
        if base == "/" {
            return true;
        }
        let dir = format!("{}/", base.trim_end_matches('/'));
        key.starts_with(&dir)
    }
}

#[async_trait]
impl FilesystemBackend for MemoryFs {
    async fn ls(&self, path: &str) -> Result<Vec<FileInfo>, FsError> {
        let files = self.files.lock().unwrap();
        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        let mut dirs = BTreeSet::new();
        let mut out = Vec::new();
        for (k, v) in files.iter() {
            let Some(rel) = k.strip_prefix(&prefix) else { continue };
            if rel.is_empty() {
                continue;
            }
            match rel.split_once('/') {
                Some((seg, _)) => {
                    dirs.insert(format!("{prefix}{seg}"));
                }
                None => out.push(FileInfo::file(k.clone(), v.len() as u64)),
            }
        }
        for d in dirs {
            out.push(FileInfo::dir(d));
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn read(&self, path: &str, offset: usize, limit: usize) -> Result<ReadResult, FsError> {
        let files = self.files.lock().unwrap();
        let content = files.get(path).ok_or_else(|| FsError::NotFound(path.to_string()))?;
        let lines: Vec<&str> = content.lines().collect();
        let end = offset.saturating_add(limit).min(lines.len());
        let slice = if offset < lines.len() { &lines[offset..end] } else { &[][..] };
        Ok(ReadResult {
            content: slice.join("\n"),
            start_line: offset + 1,
            truncated: end < lines.len(),
        })
    }

    async fn write(&self, path: &str, content: &str) -> Result<(), FsError> {
        let mut files = self.files.lock().unwrap();
        if files.contains_key(path) {
            return Err(FsError::AlreadyExists(path.to_string()));
        }
        files.insert(path.to_string(), content.to_string());
        Ok(())
    }

    async fn edit(&self, path: &str, old: &str, new: &str, replace_all: bool) -> Result<usize, FsError> {
        let mut files = self.files.lock().unwrap();
        let content = files.get(path).ok_or_else(|| FsError::NotFound(path.to_string()))?;
        let count = content.matches(old).count();
        if count == 0 {
            return Err(FsError::NoEditMatch);
        }
        if !replace_all && count > 1 {
            return Err(FsError::AmbiguousEdit(count));
        }
        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };
        files.insert(path.to_string(), updated);
        Ok(if replace_all { count } else { 1 })
    }

    async fn grep(
        &self,
        pattern: &str,
        path: Option<&str>,
        _glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>, FsError> {
        let files = self.files.lock().unwrap();
        let base = path.unwrap_or("/");
        let mut out = Vec::new();
        for (k, v) in files.iter() {
            if !Self::under(base, k) {
                continue;
            }
            for (i, line) in v.lines().enumerate() {
                if line.contains(pattern) {
                    out.push(GrepMatch { path: k.clone(), line: (i + 1) as u32, text: line.to_string() });
                }
            }
        }
        Ok(out)
    }

    async fn glob(&self, pattern: &str, path: Option<&str>) -> Result<Vec<FileInfo>, FsError> {
        // Tiny matcher: exact, `*.ext` (suffix), and `**/*.ext`.
        let files = self.files.lock().unwrap();
        let base = path.unwrap_or("/");
        let suffix = pattern
            .strip_prefix("**/*")
            .or_else(|| pattern.strip_prefix("*"))
            .map(str::to_string);
        let mut out = Vec::new();
        for (k, v) in files.iter() {
            if !Self::under(base, k) {
                continue;
            }
            let hit = match &suffix {
                Some(s) => k.ends_with(s.as_str()),
                None => k.trim_start_matches('/') == pattern.trim_start_matches('/'),
            };
            if hit {
                out.push(FileInfo::file(k.clone(), v.len() as u64));
            }
        }
        Ok(out)
    }

    async fn delete(&self, path: &str) -> Result<(), FsError> {
        if self.files.lock().unwrap().remove(path).is_some() {
            Ok(())
        } else {
            Err(FsError::NotFound(path.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_read_delete_roundtrip() {
        let fs = MemoryFs::new();
        fs.write("/a.txt", "hello\nworld").await.unwrap();
        assert!(matches!(fs.write("/a.txt", "x").await, Err(FsError::AlreadyExists(_))));
        assert_eq!(fs.read("/a.txt", 0, usize::MAX).await.unwrap().content, "hello\nworld");
        fs.delete("/a.txt").await.unwrap();
        assert!(matches!(fs.read("/a.txt", 0, 1).await, Err(FsError::NotFound(_))));
    }

    #[tokio::test]
    async fn read_whole_file_with_max_limit() {
        let fs = MemoryFs::seeded(&[("/m.md", "one\n§\ntwo")]);
        assert_eq!(fs.read("/m.md", 0, usize::MAX).await.unwrap().content, "one\n§\ntwo");
    }
}
