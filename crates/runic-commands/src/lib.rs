//! `runic-commands` — user slash commands as Markdown (`COMMAND.md`).
//!
//! A command is a reusable **prompt template** the user invokes as `/name args`
//! from whatever surface hosts the agent (REPL, HTTP, …). It does NOT bypass
//! the model and it is NOT a tool: the surface parses `/name args`, expands the
//! template, and sends the result as the run's user input.
//!
//! ```text
//! commands/review/COMMAND.md
//! ```
//! ```markdown
//! ---
//! name: review
//! description: Review a file for bugs and style
//! ---
//! Review the following file and list issues:
//!
//! $ARGUMENTS
//! ```

use std::path::Path;

use serde::Deserialize;

/// A parsed command (a named prompt template).
#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub description: String,
    /// The template body; `$ARGUMENTS` is substituted at expansion.
    pub body: String,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: String,
    #[serde(default)]
    description: String,
}

impl Command {
    /// Parse a `COMMAND.md` document.
    pub fn parse(src: &str) -> anyhow::Result<Self> {
        let src = src.trim_start_matches('\u{feff}').trim_start();
        let rest = src
            .strip_prefix("---")
            .ok_or_else(|| anyhow::anyhow!("COMMAND.md must start with `---` frontmatter"))?;
        let end = rest
            .find("\n---")
            .ok_or_else(|| anyhow::anyhow!("COMMAND.md frontmatter is not terminated by `---`"))?;
        let fm: Frontmatter = serde_yml::from_str(&rest[..end])
            .map_err(|e| anyhow::anyhow!("invalid COMMAND.md frontmatter: {e}"))?;
        if fm.name.trim().is_empty() {
            anyhow::bail!("COMMAND.md frontmatter is missing `name`");
        }
        let after = &rest[end + 4..];
        let body = after.strip_prefix('\n').unwrap_or(after).trim().to_string();
        Ok(Self {
            name: fm.name,
            description: fm.description,
            body,
        })
    }

    /// Expand the template with the user's arguments. `$ARGUMENTS` is replaced
    /// if present; otherwise args (if any) are appended so they're never lost.
    pub fn expand(&self, args: &str) -> String {
        if self.body.contains("$ARGUMENTS") {
            self.body.replace("$ARGUMENTS", args)
        } else if args.trim().is_empty() {
            self.body.clone()
        } else {
            format!("{}\n\n{args}", self.body.trim_end())
        }
    }
}

/// Split a raw input into `(command_name, args)` if it's a `/name ...`
/// invocation, else `None`.
pub fn split_invocation(input: &str) -> Option<(&str, &str)> {
    let rest = input.trim_start().strip_prefix('/')?;
    let rest = rest.trim_start_matches('/'); // tolerate "//"
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (rest, ""),
    };
    if name.is_empty() {
        None
    } else {
        Some((name, args))
    }
}

/// A set of commands.
#[derive(Debug, Clone, Default)]
pub struct CommandRegistry {
    commands: Vec<Command>,
}

impl CommandRegistry {
    pub fn new(commands: Vec<Command>) -> Self {
        Self { commands }
    }

    /// Load every `<root>/<name>/COMMAND.md` and any top-level `<root>/*.md`.
    pub fn from_dir(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut commands = Vec::new();
        let mut consider = |path: &Path| {
            if let Ok(text) = std::fs::read_to_string(path) {
                match Command::parse(&text) {
                    Ok(cmd) => commands.push(cmd),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "skipping invalid COMMAND.md")
                    }
                }
            }
        };
        for entry in std::fs::read_dir(root)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let cmd = path.join("COMMAND.md");
                if cmd.is_file() {
                    consider(&cmd);
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                consider(&path);
            }
        }
        Ok(Self { commands })
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
    pub fn len(&self) -> usize {
        self.commands.len()
    }
    pub fn get(&self, name: &str) -> Option<&Command> {
        self.commands.iter().find(|c| c.name == name)
    }
    pub fn names(&self) -> Vec<&str> {
        self.commands.iter().map(|c| c.name.as_str()).collect()
    }

    /// All commands (for aggregation, e.g. by the plugin manager).
    pub fn all(&self) -> &[Command] {
        &self.commands
    }

    /// Resolve a raw `/name args` input to its expanded prompt, if it's a
    /// known command. Returns `None` for non-invocations and unknown commands
    /// (so the surface can fall through to a plain message).
    pub fn resolve(&self, input: &str) -> Option<String> {
        let (name, args) = split_invocation(input)?;
        self.get(name).map(|cmd| cmd.expand(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(src: &str) -> Command {
        Command::parse(src).unwrap()
    }

    #[test]
    fn parses_and_expands_arguments() {
        let c = cmd("---\nname: review\ndescription: review a file\n---\nReview this:\n\n$ARGUMENTS");
        assert_eq!(c.name, "review");
        assert_eq!(c.expand("src/lib.rs"), "Review this:\n\nsrc/lib.rs");
    }

    #[test]
    fn appends_args_when_no_placeholder() {
        let c = cmd("---\nname: summarize\ndescription: x\n---\nSummarize the input.");
        assert_eq!(c.expand("hello"), "Summarize the input.\n\nhello");
        assert_eq!(c.expand(""), "Summarize the input.");
    }

    #[test]
    fn split_invocation_parses() {
        assert_eq!(split_invocation("/review src/lib.rs"), Some(("review", "src/lib.rs")));
        assert_eq!(split_invocation("/help"), Some(("help", "")));
        assert_eq!(split_invocation("not a command"), None);
        assert_eq!(split_invocation("/"), None);
    }

    #[test]
    fn registry_resolves_known_only() {
        let reg = CommandRegistry::new(vec![cmd(
            "---\nname: review\ndescription: x\n---\nReview:\n$ARGUMENTS",
        )]);
        assert_eq!(reg.resolve("/review foo.rs").as_deref(), Some("Review:\nfoo.rs"));
        assert!(reg.resolve("/unknown x").is_none());
        assert!(reg.resolve("plain message").is_none());
    }
}
