//! [`CompositeBackend`] — route filesystem operations to different backends by
//! path prefix, and **aggregate** read-only operations (`ls`/`grep`/`glob`)
//! across every mount so the agent sees one unified filesystem.
//!
//! ```ignore
//! let fs = CompositeBackend::new(default)            // "/"          → default
//!     .mount("/memories/", gcs)                      // "/memories/" → gcs
//!     .mount("/s3/", s3);                            // "/s3/"       → s3
//! ```

use std::sync::Arc;

use async_trait::async_trait;

use crate::{FileInfo, FilesystemBackend, FsError, GrepMatch, ReadResult};

/// Routes file ops to mounted backends by longest-matching path prefix, with a
/// default for everything else.
pub struct CompositeBackend {
    default: Arc<dyn FilesystemBackend>,
    /// `(prefix-with-trailing-slash, backend)`, sorted by prefix length desc.
    routes: Vec<(String, Arc<dyn FilesystemBackend>)>,
}

impl CompositeBackend {
    /// A composite whose unmatched paths go to `default`.
    pub fn new(default: Arc<dyn FilesystemBackend>) -> Self {
        Self { default, routes: Vec::new() }
    }

    /// Mount `backend` at `prefix` (e.g. `"/memories/"`). Builder-style.
    pub fn mount(mut self, prefix: impl Into<String>, backend: Arc<dyn FilesystemBackend>) -> Self {
        let mut prefix = prefix.into();
        if !prefix.starts_with('/') {
            prefix.insert(0, '/');
        }
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        self.routes.push((prefix, backend));
        // Longest prefix first so `/a/b/` wins over `/a/`.
        self.routes.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        self
    }

    /// The mount prefixes (for tests / introspection).
    pub fn mounts(&self) -> Vec<&str> {
        self.routes.iter().map(|(p, _)| p.as_str()).collect()
    }

    /// Resolve a path to `(backend, path-relative-to-backend, matched-prefix)`.
    /// `matched-prefix` is `None` when the default backend is used.
    fn route(&self, path: &str) -> (&Arc<dyn FilesystemBackend>, String, Option<String>) {
        for (prefix, backend) in &self.routes {
            let bare = prefix.trim_end_matches('/');
            if path == bare {
                return (backend, "/".to_string(), Some(prefix.clone()));
            }
            if let Some(suffix) = path.strip_prefix(prefix.as_str()) {
                let inner = if suffix.is_empty() {
                    "/".to_string()
                } else {
                    format!("/{suffix}")
                };
                return (backend, inner, Some(prefix.clone()));
            }
        }
        (&self.default, path.to_string(), None)
    }
}

/// Prepend a mount prefix to a path the mounted backend returned.
fn remap(prefix: &str, inner: &str) -> String {
    format!("{}{}", prefix.trim_end_matches('/'), inner)
}

fn remap_info(prefix: &str, mut fi: FileInfo) -> FileInfo {
    fi.path = remap(prefix, &fi.path);
    fi
}

fn remap_match(prefix: &str, mut m: GrepMatch) -> GrepMatch {
    m.path = remap(prefix, &m.path);
    m
}

/// Strip a mount prefix from a glob pattern when the pattern targets that mount
/// (so the mounted backend sees a pattern relative to its own root).
fn strip_route_from_pattern(pattern: &str, prefix: &str) -> String {
    let bare_pattern = pattern.trim_start_matches('/');
    let bare_prefix = format!("{}/", prefix.trim_matches('/'));
    if let Some(rest) = bare_pattern.strip_prefix(&bare_prefix) {
        rest.to_string()
    } else {
        pattern.to_string()
    }
}

#[async_trait]
impl FilesystemBackend for CompositeBackend {
    async fn ls(&self, path: &str) -> Result<Vec<FileInfo>, FsError> {
        let (backend, inner, matched) = self.route(path);
        if let Some(prefix) = matched {
            let entries = backend.ls(&inner).await?;
            return Ok(entries.into_iter().map(|fi| remap_info(&prefix, fi)).collect());
        }
        // At root, aggregate the default's entries + each mount as a virtual dir.
        if path == "/" {
            let mut out = self.default.ls("/").await?;
            for (prefix, _) in &self.routes {
                out.push(FileInfo::dir(prefix.clone()));
            }
            out.sort_by(|a, b| a.path.cmp(&b.path));
            return Ok(out);
        }
        self.default.ls(path).await
    }

    async fn read(&self, path: &str, offset: usize, limit: usize) -> Result<ReadResult, FsError> {
        let (backend, inner, _) = self.route(path);
        backend.read(&inner, offset, limit).await
    }

    async fn write(&self, path: &str, content: &str) -> Result<(), FsError> {
        let (backend, inner, _) = self.route(path);
        backend.write(&inner, content).await
    }

    async fn edit(
        &self,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize, FsError> {
        let (backend, inner, _) = self.route(path);
        backend.edit(&inner, old, new, replace_all).await
    }

    async fn delete(&self, path: &str) -> Result<(), FsError> {
        let (backend, inner, _) = self.route(path);
        backend.delete(&inner).await
    }

    async fn grep(
        &self,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>, FsError> {
        // A path that targets a specific mount: delegate + remap.
        if let Some(p) = path {
            let (backend, inner, matched) = self.route(p);
            if let Some(prefix) = matched {
                let matches = backend.grep(pattern, Some(&inner), glob).await?;
                return Ok(matches.into_iter().map(|m| remap_match(&prefix, m)).collect());
            }
        }
        // Whole-tree (None or "/"): fan out across default + every mount.
        if path.is_none() || path == Some("/") {
            let mut all = self.default.grep(pattern, path, glob).await?;
            for (prefix, backend) in &self.routes {
                let matches = backend.grep(pattern, Some("/"), glob).await?;
                all.extend(matches.into_iter().map(|m| remap_match(prefix, m)));
            }
            return Ok(all);
        }
        // A specific path that matched no mount: default only.
        self.default.grep(pattern, path, glob).await
    }

    async fn glob(&self, pattern: &str, path: Option<&str>) -> Result<Vec<FileInfo>, FsError> {
        if let Some(p) = path {
            let (backend, inner, matched) = self.route(p);
            if let Some(prefix) = matched {
                let found = backend.glob(pattern, Some(&inner)).await?;
                return Ok(found.into_iter().map(|fi| remap_info(&prefix, fi)).collect());
            }
        }
        // Fan out: default + every mount (with the prefix stripped from the pattern).
        let mut out = self.default.glob(pattern, path).await?;
        for (prefix, backend) in &self.routes {
            let route_pattern = strip_route_from_pattern(pattern, prefix);
            let found = backend.glob(&route_pattern, Some("/")).await?;
            out.extend(found.into_iter().map(|fi| remap_info(prefix, fi)));
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// A minimal in-memory [`FilesystemBackend`] used only to exercise the
    /// composite's routing/aggregation/remapping. (`MemoryFs` ships in its
    /// own crate; this is a test fixture.)
    struct TestFs {
        files: Mutex<BTreeMap<String, String>>,
    }

    impl TestFs {
        fn seeded(entries: &[(&str, &str)]) -> Arc<Self> {
            let mut m = BTreeMap::new();
            for (k, v) in entries {
                m.insert((*k).to_string(), (*v).to_string());
            }
            Arc::new(Self { files: Mutex::new(m) })
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
    impl FilesystemBackend for TestFs {
        async fn ls(&self, path: &str) -> Result<Vec<FileInfo>, FsError> {
            let files = self.files.lock().unwrap();
            let prefix = if path == "/" {
                "/".to_string()
            } else {
                format!("{}/", path.trim_end_matches('/'))
            };
            let mut dirs = std::collections::BTreeSet::new();
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
            let end = (offset + limit).min(lines.len());
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

        async fn edit(
            &self,
            path: &str,
            old: &str,
            new: &str,
            replace_all: bool,
        ) -> Result<usize, FsError> {
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
            // Tiny matcher: support exact, `*.ext` (suffix), and `**/*.ext`.
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

    fn paths(v: &[FileInfo]) -> Vec<String> {
        v.iter().map(|f| f.path.clone()).collect()
    }

    // ── routing ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn route_picks_default_and_mount() {
        let default = TestFs::seeded(&[("/scratch.txt", "x")]);
        let mem = TestFs::seeded(&[("/note.md", "remember me")]);
        let fs = CompositeBackend::new(default).mount("/memories/", mem.clone());

        // default path
        assert_eq!(fs.read("/scratch.txt", 0, 100).await.unwrap().content, "x");
        // mounted path → backend receives the STRIPPED path "/note.md"
        assert_eq!(fs.read("/memories/note.md", 0, 100).await.unwrap().content, "remember me");
    }

    #[tokio::test]
    async fn route_longest_prefix_wins() {
        let shallow = TestFs::seeded(&[("/x", "shallow")]);
        let deep = TestFs::seeded(&[("/x", "deep")]);
        let fs = CompositeBackend::new(TestFs::seeded(&[]))
            .mount("/a/", shallow)
            .mount("/a/b/", deep);
        assert_eq!(fs.mounts(), vec!["/a/b/", "/a/"]); // sorted longest-first
        assert_eq!(fs.read("/a/b/x", 0, 9).await.unwrap().content, "deep");
        assert_eq!(fs.read("/a/x", 0, 9).await.unwrap().content, "shallow");
    }

    #[tokio::test]
    async fn route_exact_prefix_path_maps_to_root() {
        let mem = TestFs::seeded(&[("/top.md", "hi")]);
        let fs = CompositeBackend::new(TestFs::seeded(&[])).mount("/m/", mem);
        // "/m" (no trailing slash) → that backend, ls "/"
        let listed = fs.ls("/m").await.unwrap();
        assert_eq!(paths(&listed), vec!["/m/top.md"]);
    }

    // ── writes/edits route to the right backend ─────────────────────────────

    #[tokio::test]
    async fn write_and_edit_land_in_the_mounted_backend() {
        let mem = TestFs::seeded(&[]);
        let fs = CompositeBackend::new(TestFs::seeded(&[])).mount("/m/", mem.clone());

        fs.write("/m/todo.md", "buy milk\nbuy milk").await.unwrap();
        // The mounted backend stored it under its own root ("/todo.md").
        assert!(mem.files.lock().unwrap().contains_key("/todo.md"));

        // edit with a non-unique string requires replace_all
        assert_eq!(
            fs.edit("/m/todo.md", "milk", "eggs", false).await,
            Err(FsError::AmbiguousEdit(2))
        );
        assert_eq!(fs.edit("/m/todo.md", "milk", "eggs", true).await.unwrap(), 2);
        assert_eq!(fs.read("/m/todo.md", 0, 10).await.unwrap().content, "buy eggs\nbuy eggs");
    }

    #[tokio::test]
    async fn write_conflict_is_propagated() {
        let fs = CompositeBackend::new(TestFs::seeded(&[("/a.txt", "x")]));
        assert_eq!(fs.write("/a.txt", "y").await, Err(FsError::AlreadyExists("/a.txt".into())));
    }

    // ── ls aggregation + virtual dirs + remap ───────────────────────────────

    #[tokio::test]
    async fn ls_root_aggregates_default_and_virtual_mount_dirs() {
        let default = TestFs::seeded(&[("/a.txt", "x"), ("/b.txt", "y")]);
        let fs = CompositeBackend::new(default)
            .mount("/memories/", TestFs::seeded(&[("/n.md", "")]))
            .mount("/s3/", TestFs::seeded(&[]));
        let listed = fs.ls("/").await.unwrap();
        // default files + a virtual dir per mount, sorted
        assert_eq!(paths(&listed), vec!["/a.txt", "/b.txt", "/memories/", "/s3/"]);
        assert!(listed.iter().find(|f| f.path == "/memories/").unwrap().is_dir);
    }

    #[tokio::test]
    async fn ls_of_a_mount_remaps_entries_into_unified_namespace() {
        let mem = TestFs::seeded(&[("/a.md", "x"), ("/sub/b.md", "y")]);
        let fs = CompositeBackend::new(TestFs::seeded(&[])).mount("/memories/", mem);
        let listed = fs.ls("/memories/").await.unwrap();
        // a file + a synthesized subdir, both prefixed back to /memories/...
        assert_eq!(paths(&listed), vec!["/memories/a.md", "/memories/sub"]);
    }

    // ── grep fan-out + remap ────────────────────────────────────────────────

    #[tokio::test]
    async fn grep_root_fans_out_across_all_mounts_and_remaps() {
        let default = TestFs::seeded(&[("/d.txt", "TODO default")]);
        let m1 = TestFs::seeded(&[("/x.md", "a line\nTODO in memories")]);
        let m2 = TestFs::seeded(&[("/y.md", "TODO in s3")]);
        let fs = CompositeBackend::new(default).mount("/memories/", m1).mount("/s3/", m2);

        let mut hits = fs.grep("TODO", Some("/"), None).await.unwrap();
        hits.sort_by(|a, b| a.path.cmp(&b.path));
        let paths: Vec<_> = hits.iter().map(|m| m.path.clone()).collect();
        assert_eq!(paths, vec!["/d.txt", "/memories/x.md", "/s3/y.md"]);
        // line numbers are preserved through remap
        assert_eq!(hits.iter().find(|m| m.path == "/memories/x.md").unwrap().line, 2);
    }

    #[tokio::test]
    async fn grep_scoped_to_a_mount_only_searches_that_mount() {
        let default = TestFs::seeded(&[("/d.txt", "TODO default")]);
        let mem = TestFs::seeded(&[("/x.md", "TODO mem")]);
        let fs = CompositeBackend::new(default).mount("/memories/", mem);
        let hits = fs.grep("TODO", Some("/memories/"), None).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/memories/x.md");
    }

    #[tokio::test]
    async fn grep_none_path_also_fans_out() {
        let fs = CompositeBackend::new(TestFs::seeded(&[("/d", "hit")]))
            .mount("/m/", TestFs::seeded(&[("/f", "hit")]));
        let hits = fs.grep("hit", None, None).await.unwrap();
        assert_eq!(hits.len(), 2);
    }

    // ── glob fan-out + pattern strip + remap ────────────────────────────────

    #[tokio::test]
    async fn glob_fans_out_and_remaps() {
        let default = TestFs::seeded(&[("/a.md", ""), ("/b.txt", "")]);
        let mem = TestFs::seeded(&[("/c.md", "")]);
        let fs = CompositeBackend::new(default).mount("/memories/", mem);
        let found = fs.glob("**/*.md", None).await.unwrap();
        assert_eq!(paths(&found), vec!["/a.md", "/memories/c.md"]);
    }

    #[tokio::test]
    async fn glob_scoped_to_mount_remaps() {
        let mem = TestFs::seeded(&[("/c.md", ""), ("/d.txt", "")]);
        let fs = CompositeBackend::new(TestFs::seeded(&[])).mount("/memories/", mem);
        let found = fs.glob("*.md", Some("/memories/")).await.unwrap();
        assert_eq!(paths(&found), vec!["/memories/c.md"]);
    }

    #[tokio::test]
    async fn strip_route_from_pattern_works() {
        assert_eq!(strip_route_from_pattern("/memories/**/*.md", "/memories/"), "**/*.md");
        assert_eq!(strip_route_from_pattern("**/*.md", "/memories/"), "**/*.md");
        assert_eq!(strip_route_from_pattern("memories/x.md", "/memories/"), "x.md");
    }

    // ── no-mount composite behaves like its default ─────────────────────────

    #[tokio::test]
    async fn composite_with_no_mounts_is_transparent() {
        let fs = CompositeBackend::new(TestFs::seeded(&[("/a.txt", "hello")]));
        assert_eq!(fs.read("/a.txt", 0, 10).await.unwrap().content, "hello");
        assert_eq!(paths(&fs.ls("/").await.unwrap()), vec!["/a.txt"]);
        assert!(fs.read("/missing", 0, 10).await.is_err());
    }
}
