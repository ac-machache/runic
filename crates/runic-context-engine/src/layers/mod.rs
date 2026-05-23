//! Built-in `ContextLayer` impls.

pub mod base;
pub mod file;
pub mod memory;
pub mod persona;
pub mod user;

pub use base::BasePromptLayer;
pub use file::FileLayer;
pub use memory::MemoryLayer;
pub use persona::PersonaLayer;
pub use user::UserFactsLayer;

/// Trim `content`; if non-empty, wrap it in `<{tag}>...</{tag}>`. Optionally
/// prepend `preamble` (one blank line between preamble and content) so the
/// model knows what the block IS. Returns `None` for empty content so empty
/// blocks never pollute the assembled prompt.
pub(crate) fn wrap_block(tag: &str, preamble: Option<&str>, content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut body = String::new();
    if let Some(p) = preamble {
        let p = p.trim();
        if !p.is_empty() {
            body.push_str(p);
            body.push_str("\n\n");
        }
    }
    body.push_str(trimmed);

    Some(format!("<{tag}>\n{body}\n</{tag}>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omits_empty_content() {
        assert!(wrap_block("persona", None, "   \n  ").is_none());
        assert!(wrap_block("persona", None, "").is_none());
    }

    #[test]
    fn wraps_without_preamble() {
        let out = wrap_block("persona", None, "  hello  ").unwrap();
        assert_eq!(out, "<persona>\nhello\n</persona>");
    }

    #[test]
    fn wraps_with_preamble() {
        let out = wrap_block("persona", Some("Your persona."), "hello").unwrap();
        assert_eq!(out, "<persona>\nYour persona.\n\nhello\n</persona>");
    }

    #[test]
    fn empty_preamble_is_treated_as_none() {
        let out = wrap_block("persona", Some("   "), "hello").unwrap();
        assert_eq!(out, "<persona>\nhello\n</persona>");
    }
}
