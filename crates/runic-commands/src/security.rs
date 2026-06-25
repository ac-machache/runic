//! `COMMAND.md` input safety (mirrors `runic-skills`/`runic-subagent`).
//!
//! A command's `name`/`description` surface in the slash-command UI and prompt,
//! so sanitize + bound them. The name additionally must be a single
//! whitespace-free token — `split_invocation` splits `/name args` on the first
//! whitespace, so a name with a space is uninvokable.

/// Max chars for a command `name`.
pub const MAX_NAME: usize = 64;
/// Max chars for a command `description`.
pub const MAX_DESCRIPTION: usize = 1024;

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

/// A slash-command name must be a single, whitespace-free, non-empty token
/// (so `/name` is reachable), with no leading `/`, within the length cap.
pub fn validate_command_name(name: &str) -> anyhow::Result<()> {
    validate_len(name, MAX_NAME, "name")?;
    if name.starts_with('/') {
        anyhow::bail!("command name must not start with `/`");
    }
    if name.chars().any(char::is_whitespace) {
        anyhow::bail!("command name must be a single token (no whitespace)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_collapses_whitespace() {
        assert_eq!(sanitize_single_line("a\n b\tc"), "a b c");
    }

    #[test]
    fn command_name_rules() {
        assert!(validate_command_name("review").is_ok());
        assert!(validate_command_name("two words").is_err());
        assert!(validate_command_name("/slashy").is_err());
        assert!(validate_command_name("").is_err());
        assert!(validate_command_name(&"x".repeat(65)).is_err());
    }
}
