//! `Dirs` — flexible directory-path input for loaders: accept one path, a
//! `Vec` of paths, or a `HashMap` whose values are paths, and normalize to
//! `Vec<PathBuf>`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub trait Dirs {
    fn dirs(self) -> Vec<PathBuf>;
}

impl Dirs for &str {
    fn dirs(self) -> Vec<PathBuf> {
        vec![self.into()]
    }
}
impl Dirs for String {
    fn dirs(self) -> Vec<PathBuf> {
        vec![self.into()]
    }
}
impl Dirs for &Path {
    fn dirs(self) -> Vec<PathBuf> {
        vec![self.to_path_buf()]
    }
}
impl Dirs for PathBuf {
    fn dirs(self) -> Vec<PathBuf> {
        vec![self]
    }
}
impl<P: Into<PathBuf>> Dirs for Vec<P> {
    fn dirs(self) -> Vec<PathBuf> {
        self.into_iter().map(Into::into).collect()
    }
}
impl<K, P: Into<PathBuf>> Dirs for HashMap<K, P> {
    fn dirs(self) -> Vec<PathBuf> {
        self.into_values().map(Into::into).collect()
    }
}
