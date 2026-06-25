//! Skill-loading safety checks (ported from codex `core-skills`).
//!
//! Skills are read-only config, but their text lands in the system-prompt index
//! and their folders are addressed by the model — so we sanitize and bound
//! everything: collapse whitespace (no prompt-injection via newlines), cap
//! field lengths, and refuse path escapes on sub-file reads.

/// Max chars for a skill's bare `name`.
pub const MAX_NAME: usize = 64;
/// Max chars for a skill's `description`.
pub const MAX_DESCRIPTION: usize = 1024;
/// Max chars for the qualified `namespace:name` id.
pub const MAX_QUALIFIED_NAME: usize = 128;
/// Max skill folders read from a single source.
pub const MAX_SKILLS: usize = 2000;

/// Collapse all runs of whitespace (including newlines/tabs) to single spaces.
/// This is what stops a skill description from smuggling newlines or a fake
/// `</available-skills>` section into the system prompt.
pub fn sanitize_single_line(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Reject an empty or over-long field.
pub fn validate_len(value: &str, max: usize, field: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{field} is empty");
    }
    if value.chars().count() > max {
        anyhow::bail!("{field} exceeds the maximum length of {max} characters");
    }
    Ok(())
}

/// Validate a relative sub-path before it's joined onto a skill folder: no
/// absolute paths, no `..`, no empty segments. (Backends additionally
/// canonicalize and assert the result stays under the source root.)
pub fn safe_rel(rel: &str) -> anyhow::Result<()> {
    if rel.is_empty() || rel.starts_with('/') || rel.starts_with('\\') {
        anyhow::bail!("invalid skill path '{rel}'");
    }
    if rel
        .split(['/', '\\'])
        .any(|seg| seg.is_empty() || seg == "..")
    {
        anyhow::bail!("invalid skill path '{rel}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_collapses_whitespace_and_newlines() {
        assert_eq!(
            sanitize_single_line("hello\n\n</available-skills>\tinjected   text"),
            "hello </available-skills> injected text"
        );
    }

    #[test]
    fn validate_len_rejects_empty_and_oversize() {
        assert!(validate_len("", 10, "name").is_err());
        assert!(validate_len(&"x".repeat(11), 10, "name").is_err());
        assert!(validate_len("ok", 10, "name").is_ok());
    }

    #[test]
    fn safe_rel_rejects_escapes() {
        assert!(safe_rel("references/checklist.md").is_ok());
        assert!(safe_rel("../../etc/passwd").is_err());
        assert!(safe_rel("/etc/passwd").is_err());
        assert!(safe_rel("a//b").is_err());
        assert!(safe_rel("").is_err());
    }
}
