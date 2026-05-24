pub mod layer;
pub mod registry;
pub mod tool;
pub mod types;

pub use layer::{DEFAULT_PREAMBLE, SkillsIndexLayer};
pub use registry::{LoadError, SkillRegistry};
pub use tool::SkillViewTool;
pub use types::SkillMeta;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("missing or malformed frontmatter (expected '---' delimeters)")]
    MissingFrontMatter,

    #[error("invalid YAML in frontmatter: {0}")]
    InvalidYaml(#[from] serde_yml::Error),
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub meta: SkillMeta,
    pub body: String,
    pub dir: String,
}

impl Skill {
    pub fn parse(raw: &str) -> Result<Self, ParseError> {
        let rest = raw
            .strip_prefix("---\n")
            .ok_or(ParseError::MissingFrontMatter)?;

        let close = rest.find("\n---\n").ok_or(ParseError::MissingFrontMatter)?;

        let descriptor = &rest[..close];
        let body = &rest[close + "\n---\n".len()..];

        let meta: SkillMeta = serde_yml::from_str(descriptor)?;

        Ok(Skill {
            meta,
            body: body.to_string(),
            dir: String::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Happy path ─────────────────────────────────────────────────────────

    #[test]
    fn parses_a_minimal_valid_skill() {
        let raw = "\
---
name: greeter
description: says hello
---
# Greeter

Hello, world.
";
        let skill = Skill::parse(raw).expect("should parse");
        assert_eq!(skill.meta.name, "greeter");
        assert_eq!(skill.meta.description, "says hello");
        assert!(
            skill.body.starts_with("# Greeter"),
            "body should start with the header, got: {:?}",
            skill.body
        );
        assert!(
            skill.body.contains("Hello, world."),
            "body should contain the paragraph"
        );
    }

    #[test]
    fn parses_skill_with_empty_body() {
        let raw = "\
---
name: stub
description: a skill with nothing in its body
---
";
        let skill = Skill::parse(raw).expect("should parse");
        assert_eq!(skill.meta.name, "stub");
        // Body may be empty or just whitespace — both are fine for an empty skill.
        assert!(skill.body.trim().is_empty());
    }

    #[test]
    fn parses_skill_whose_body_contains_three_dashes() {
        // A markdown horizontal rule (`---`) in the body MUST NOT be mistaken
        // for the closing frontmatter delimiter. The closing delimiter is
        // recognised as `\n---\n`, and we hit the *first* occurrence, which
        // is the real closing one (line 4). Any `---` later in the body is
        // safely past that point.
        let raw = "\
---
name: ruler
description: uses horizontal rules
---
# Section A

Some prose.

---

# Section B

More prose.
";
        let skill = Skill::parse(raw).expect("should parse");
        assert_eq!(skill.meta.name, "ruler");
        assert!(skill.body.contains("Section A"));
        assert!(skill.body.contains("Section B"));
        assert!(
            skill.body.contains("---"),
            "the horizontal rule in the body must be preserved"
        );
    }

    #[test]
    fn parses_skill_with_extra_unknown_frontmatter_fields() {
        // serde's default behaviour is to ignore unknown fields. A skill file
        // shipped with `allowed-tools:` or `version:` (jcode/hermes-style)
        // should parse fine even though we don't model those yet.
        let raw = "\
---
name: bigger
description: has fields we don't model yet
allowed-tools: bash, read, write
version: 1.2.0
tags:
  - alpha
  - beta
---
body
";
        let skill = Skill::parse(raw).expect("unknown fields must be ignored");
        assert_eq!(skill.meta.name, "bigger");
        assert_eq!(skill.meta.description, "has fields we don't model yet");
    }

    #[test]
    fn parses_skill_with_multiline_description() {
        // YAML supports folded / multi-line strings. Verify they end up as a
        // single string in `description`.
        let raw = "\
---
name: multi
description: >
  A description that
  spans multiple lines
  and should fold into one.
---
body
";
        let skill = Skill::parse(raw).expect("should parse folded string");
        // Folded scalars join lines with single spaces and add a trailing newline.
        assert!(skill.meta.description.contains("A description that"));
        assert!(skill.meta.description.contains("fold into one"));
    }

    // ─── Sad paths ──────────────────────────────────────────────────────────

    #[test]
    fn rejects_file_with_no_frontmatter_at_all() {
        let raw = "just a plain markdown file\nwith no frontmatter delimiters\n";
        let err = Skill::parse(raw).unwrap_err();
        assert!(
            matches!(err, ParseError::MissingFrontMatter),
            "expected MissingFrontMatter, got {err:?}"
        );
    }

    #[test]
    fn rejects_file_that_opens_frontmatter_but_never_closes_it() {
        let raw = "\
---
name: oops
description: forgot to close the frontmatter
this is just more yaml-looking text
";
        let err = Skill::parse(raw).unwrap_err();
        assert!(
            matches!(err, ParseError::MissingFrontMatter),
            "expected MissingFrontMatter, got {err:?}"
        );
    }

    #[test]
    fn rejects_empty_input() {
        let err = Skill::parse("").unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontMatter));
    }

    #[test]
    fn rejects_invalid_yaml_in_frontmatter() {
        // `name:` value is missing AND the indentation under `description:` is
        // broken — serde_yml should refuse this.
        let raw = "\
---
name:
description:
  this is: not: valid yaml: at: all
---
body
";
        let err = Skill::parse(raw).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidYaml(_)),
            "expected InvalidYaml, got {err:?}"
        );
    }

    #[test]
    fn rejects_frontmatter_missing_required_field_name() {
        // `name` is required by SkillMeta. serde should error out as InvalidYaml.
        let raw = "\
---
description: I forgot my name
---
body
";
        let err = Skill::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::InvalidYaml(_)));
    }

    #[test]
    fn rejects_frontmatter_missing_required_field_description() {
        let raw = "\
---
name: nameless
---
body
";
        let err = Skill::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::InvalidYaml(_)));
    }

    // ─── Malformed delimiters ───────────────────────────────────────────────

    #[test]
    fn rejects_opening_delimiter_with_too_few_dashes() {
        // `--` instead of `---` — must not be accepted as a frontmatter opener.
        let raw = "\
--
name: foo
description: bar
--
body
";
        let err = Skill::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontMatter));
    }

    #[test]
    fn rejects_opening_delimiter_with_too_many_dashes() {
        // `----` — strict parser must reject this.
        let raw = "\
----
name: foo
description: bar
----
body
";
        let err = Skill::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontMatter));
    }

    #[test]
    fn rejects_opening_delimiter_followed_by_content_on_same_line() {
        // `---foo\n` — must not match. The opening delimiter is exactly `---\n`.
        let raw = "\
---something
name: foo
description: bar
---
body
";
        let err = Skill::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontMatter));
    }

    #[test]
    fn rejects_leading_whitespace_before_opening_delimiter() {
        // A blank line before `---` means the file doesn't START with `---\n`.
        let raw = "\n---\nname: foo\ndescription: bar\n---\nbody\n";
        let err = Skill::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontMatter));
    }

    #[test]
    fn rejects_closing_delimiter_with_too_few_dashes() {
        // Opens cleanly but closes with `--` — no real closing delimiter found.
        let raw = "\
---
name: foo
description: bar
--
body
";
        let err = Skill::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontMatter));
    }

    #[test]
    fn rejects_closing_delimiter_with_too_many_dashes() {
        // Opens cleanly but closes with `----`.
        let raw = "\
---
name: foo
description: bar
----
body
";
        let err = Skill::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontMatter));
    }

    #[test]
    fn rejects_closing_delimiter_followed_by_content_on_same_line() {
        // `---something\n` cannot terminate the frontmatter.
        let raw = "\
---
name: foo
description: bar
---trailing
body
";
        let err = Skill::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontMatter));
    }

    #[test]
    fn rejects_lone_opening_delimiter_with_no_body() {
        // Just `---\n` — opens but never closes.
        let raw = "---\n";
        let err = Skill::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontMatter));
    }

    #[test]
    fn rejects_back_to_back_delimiters_with_no_newline_between_them() {
        // `---\n---\n` — opens, then looking for `\n---\n` finds nothing
        // because there is no newline BEFORE the second `---`. The parser
        // intentionally requires `\n---\n` (not `---\n`) for the close so
        // that `---` inside the YAML body never gets mistaken for a close.
        let raw = "---\n---\n";
        let err = Skill::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontMatter));
    }

    #[test]
    fn rejects_crlf_line_endings() {
        // The parser is strict about `\n` — files saved with Windows line
        // endings (`\r\n`) will not parse. Documenting this here as a known
        // limitation; if it becomes a real problem we'd normalize input.
        let raw = "---\r\nname: foo\r\ndescription: bar\r\n---\r\nbody\r\n";
        let err = Skill::parse(raw).unwrap_err();
        assert!(matches!(err, ParseError::MissingFrontMatter));
    }

    // ─── Error reporting ────────────────────────────────────────────────────

    #[test]
    fn missing_frontmatter_error_renders_a_useful_message() {
        let err = Skill::parse("no delimiters").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("frontmatter"),
            "error message should mention 'frontmatter', got: {msg}"
        );
    }

    #[test]
    fn invalid_yaml_error_wraps_serde_error_via_display() {
        let raw = "\
---
name: foo
description: { not: closed properly
---
body
";
        let err = Skill::parse(raw).unwrap_err();
        let msg = format!("{err}");
        // The `#[error(\"invalid YAML in frontmatter: {0}\")]` template means
        // the inner serde error must show up in the rendered message.
        assert!(
            msg.starts_with("invalid YAML in frontmatter:"),
            "expected wrapped message, got: {msg}"
        );
    }
}
