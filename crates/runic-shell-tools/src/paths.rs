//! Path normalisation + sanity checks for storage-backend keys.
//!
//! `StorageBackend` keys are abstract — they're whatever the backend
//! interprets. We still apply a few cosmetic rules so tools behave
//! predictably across backends:
//!
//! - Strip a leading `/` (we're never absolute — the backend has its
//!   own root).
//! - Strip trailing `/` so `wikis/` and `wikis` normalise the same.
//! - Reject `..` segments — even if the backend would tolerate them,
//!   they're never what the agent meant.
//!
//! Empty result means "the backend root itself".

use crate::error::ShellToolError;

/// Normalise a user-supplied path for use as a storage key.
///
/// `None` or `Some("")` → returns `""` (the root). Otherwise the input
/// gets the leading/trailing slash treatment plus a parent-segment scan.
pub fn normalise(raw: Option<&str>) -> Result<String, ShellToolError> {
    let raw = raw.unwrap_or("");
    let mut s = raw.trim();
    while let Some(stripped) = s.strip_prefix('/') {
        s = stripped;
    }
    while let Some(stripped) = s.strip_suffix('/') {
        s = stripped;
    }
    if s.split('/').any(|seg| seg == "..") {
        return Err(ShellToolError::InvalidPath {
            path: raw.to_string(),
            reason: "parent-segment '..' not allowed",
        });
    }
    Ok(s.to_string())
}

/// Join two normalised key segments with a single `/`, dropping the
/// separator when either side is empty.
pub fn join(base: &str, child: &str) -> String {
    match (base.is_empty(), child.is_empty()) {
        (true, _) => child.to_string(),
        (_, true) => base.to_string(),
        _ => format!("{base}/{child}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_none_is_root() {
        assert_eq!(normalise(None).unwrap(), "");
        assert_eq!(normalise(Some("")).unwrap(), "");
        assert_eq!(normalise(Some("   ")).unwrap(), "");
    }

    #[test]
    fn normalise_strips_leading_and_trailing_slashes() {
        assert_eq!(normalise(Some("/wikis/")).unwrap(), "wikis");
        assert_eq!(normalise(Some("//wikis///")).unwrap(), "wikis");
        assert_eq!(
            normalise(Some("/wikis/notes.md")).unwrap(),
            "wikis/notes.md"
        );
    }

    #[test]
    fn normalise_rejects_parent_segment() {
        assert!(matches!(
            normalise(Some("wikis/../etc/passwd")).unwrap_err(),
            ShellToolError::InvalidPath { .. }
        ));
        assert!(matches!(
            normalise(Some("..")).unwrap_err(),
            ShellToolError::InvalidPath { .. }
        ));
    }

    #[test]
    fn join_handles_empty_sides() {
        assert_eq!(join("", "wikis"), "wikis");
        assert_eq!(join("wikis", ""), "wikis");
        assert_eq!(join("", ""), "");
        assert_eq!(join("wikis", "notes.md"), "wikis/notes.md");
    }
}
