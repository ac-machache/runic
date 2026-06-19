//! `LocalFs` — a [`FilesystemBackend`] over a real directory.
//!
//! Virtual `/`-rooted paths map under a workspace `root`. Path-traversal is
//! refused (`..` segments, absolute escapes); the agent can only ever address
//! what's under `root`. This is the default concrete backend the fs tools run
//! on (a `CompositeBackend` mounts it + cloud backends).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::{FileInfo, FilesystemBackend, FsError, GrepMatch, ReadResult};

/// A filesystem backend rooted at `root`.
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Map a virtual `/`-rooted path to a real path under `root`, refusing
    /// traversal.
    fn resolve(&self, vpath: &str) -> Result<PathBuf, FsError> {
        let rel = vpath.trim_start_matches('/');
        if rel.split(['/', '\\']).any(|seg| seg == ".." || seg == ".") {
            return Err(FsError::InvalidPath(vpath.to_string()));
        }
        Ok(self.root.join(rel))
    }
}

fn io(e: std::io::Error) -> FsError {
    FsError::Io(e.to_string())
}

fn modified(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    meta.modified().ok().map(DateTime::<Utc>::from)
}

/// Collect every file under `dir`, paired with its virtual path.
fn walk(dir: &Path, base_vpath: &str, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let vpath = format!("{}/{}", base_vpath.trim_end_matches('/'), name);
        if path.is_dir() {
            walk(&path, &vpath, out);
        } else {
            out.push((path, vpath));
        }
    }
}

#[async_trait]
impl FilesystemBackend for LocalFs {
    async fn ls(&self, path: &str) -> Result<Vec<FileInfo>, FsError> {
        let dir = self.resolve(path)?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(io)?.flatten() {
            let meta = entry.metadata().map_err(io)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            out.push(FileInfo {
                path: format!("{}/{}", path.trim_end_matches('/'), name),
                is_dir: meta.is_dir(),
                size: meta.len(),
                modified_at: modified(&meta),
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn read(&self, path: &str, offset: usize, limit: usize) -> Result<ReadResult, FsError> {
        let real = self.resolve(path)?;
        let content = match std::fs::read_to_string(&real) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FsError::NotFound(path.to_string()))
            }
            Err(e) => return Err(io(e)),
        };
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
        let real = self.resolve(path)?;
        if real.exists() {
            return Err(FsError::AlreadyExists(path.to_string()));
        }
        if let Some(parent) = real.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        std::fs::write(&real, content).map_err(io)
    }

    async fn edit(
        &self,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize, FsError> {
        let real = self.resolve(path)?;
        let content = match std::fs::read_to_string(&real) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FsError::NotFound(path.to_string()))
            }
            Err(e) => return Err(io(e)),
        };
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
        std::fs::write(&real, updated).map_err(io)?;
        Ok(if replace_all { count } else { 1 })
    }

    async fn grep(
        &self,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>, FsError> {
        let base = path.unwrap_or("/");
        let dir = self.resolve(base)?;
        let matcher = glob
            .map(|g| globset::Glob::new(g).map(|x| x.compile_matcher()))
            .transpose()
            .map_err(|e| FsError::InvalidPath(e.to_string()))?;

        let mut files = Vec::new();
        walk(&dir, base, &mut files);
        let mut out = Vec::new();
        for (real, vpath) in files {
            if let Some(m) = &matcher
                && !m.is_match(vpath.trim_start_matches('/')) {
                    continue;
                }
            // Skip non-UTF-8 / binary files silently.
            let Ok(content) = std::fs::read_to_string(&real) else { continue };
            for (i, line) in content.lines().enumerate() {
                if line.contains(pattern) {
                    out.push(GrepMatch {
                        path: vpath.clone(),
                        line: (i + 1) as u32,
                        text: line.to_string(),
                    });
                }
            }
        }
        Ok(out)
    }

    async fn glob(&self, pattern: &str, path: Option<&str>) -> Result<Vec<FileInfo>, FsError> {
        let base = path.unwrap_or("/");
        let dir = self.resolve(base)?;
        let matcher = globset::Glob::new(pattern)
            .map(|x| x.compile_matcher())
            .map_err(|e| FsError::InvalidPath(e.to_string()))?;

        let mut files = Vec::new();
        walk(&dir, base, &mut files);
        let mut out: Vec<FileInfo> = files
            .into_iter()
            .filter(|(_, vpath)| matcher.is_match(vpath.trim_start_matches('/')))
            .filter_map(|(real, vpath)| {
                let meta = std::fs::metadata(&real).ok()?;
                Some(FileInfo {
                    path: vpath,
                    is_dir: false,
                    size: meta.len(),
                    modified_at: modified(&meta),
                })
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn delete(&self, path: &str) -> Result<(), FsError> {
        let real = self.resolve(path)?;
        if real.is_dir() {
            return Err(FsError::IsDirectory(path.to_string()));
        }
        match std::fs::remove_file(&real) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(FsError::NotFound(path.to_string()))
            }
            Err(e) => Err(io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs() -> (tempfile::TempDir, LocalFs) {
        let tmp = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(tmp.path());
        (tmp, fs)
    }

    #[tokio::test]
    async fn write_read_edit_delete_roundtrip() {
        let (_tmp, fs) = fs();
        fs.write("/a/b.txt", "hello\nworld").await.unwrap();
        // write refuses to clobber
        assert!(matches!(fs.write("/a/b.txt", "x").await, Err(FsError::AlreadyExists(_))));

        let r = fs.read("/a/b.txt", 0, 10).await.unwrap();
        assert_eq!(r.content, "hello\nworld");

        assert_eq!(fs.edit("/a/b.txt", "world", "there", false).await.unwrap(), 1);
        assert_eq!(fs.read("/a/b.txt", 0, 10).await.unwrap().content, "hello\nthere");

        fs.delete("/a/b.txt").await.unwrap();
        assert!(matches!(fs.read("/a/b.txt", 0, 10).await, Err(FsError::NotFound(_))));
    }

    #[tokio::test]
    async fn ls_glob_grep() {
        let (_tmp, fs) = fs();
        fs.write("/src/lib.rs", "fn main() {}\n// TODO: fix").await.unwrap();
        fs.write("/src/mod.rs", "pub mod x;").await.unwrap();
        fs.write("/README.md", "# hi").await.unwrap();

        let listed = fs.ls("/src").await.unwrap();
        assert_eq!(listed.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
                   vec!["/src/lib.rs", "/src/mod.rs"]);

        let rs = fs.glob("**/*.rs", None).await.unwrap();
        assert_eq!(rs.len(), 2);

        let hits = fs.grep("TODO", None, None).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/src/lib.rs");
        assert_eq!(hits[0].line, 2);
    }

    #[tokio::test]
    async fn refuses_path_traversal() {
        let (_tmp, fs) = fs();
        assert!(matches!(fs.read("/../etc/passwd", 0, 1).await, Err(FsError::InvalidPath(_))));
        assert!(matches!(fs.write("/a/../../x", "y").await, Err(FsError::InvalidPath(_))));
    }
}
