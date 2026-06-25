//! `AGENT.md` input safety (mirrors `runic-skills::security`).
//!
//! A subagent's `name`/`description` surface in the delegate roster and the
//! system prompt, so we sanitize and bound them — collapse whitespace (no
//! prompt-injection via newlines), cap field lengths, and cap/dedupe the
//! tool/skill allow-lists. (Duplicated for now; to be lifted into a shared
//! `runic-resource` during the source-model consolidation.)

/// Max chars for an agent `name`.
pub const MAX_NAME: usize = 64;
/// Max chars for an agent `description`.
pub const MAX_DESCRIPTION: usize = 1024;
/// Max chars for a `provider` override.
pub const MAX_PROVIDER: usize = 64;
/// Max chars for a `model` override.
pub const MAX_MODEL: usize = 128;
/// Max entries in an `allowed_tools` / `skills` list.
pub const MAX_LIST: usize = 256;

/// Collapse all runs of whitespace (newlines/tabs included) to single spaces.
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

/// Sanitize each entry of an identifier list (tool/skill names), drop empties,
/// dedupe (order-preserving), and cap the count.
pub fn sanitize_list(items: Vec<String>, max: usize, field: &str) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let s = sanitize_single_line(&item);
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    if out.len() > max {
        anyhow::bail!(
            "{field} has {} entries, exceeds the maximum of {max}",
            out.len()
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_collapses_whitespace() {
        assert_eq!(sanitize_single_line("a\n\nb\tc   d"), "a b c d");
    }

    #[test]
    fn validate_len_bounds() {
        assert!(validate_len("", 4, "f").is_err());
        assert!(validate_len("toolong", 4, "f").is_err());
        assert!(validate_len("ok", 4, "f").is_ok());
    }

    #[test]
    fn sanitize_list_dedupes_and_caps() {
        let got = sanitize_list(
            vec!["a".into(), " a ".into(), "".into(), "b".into()],
            10,
            "tools",
        )
        .unwrap();
        assert_eq!(got, vec!["a", "b"]);

        let many: Vec<String> = (0..5).map(|i| i.to_string()).collect();
        assert!(sanitize_list(many, 3, "tools").is_err());
    }
}
