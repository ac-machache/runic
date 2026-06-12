//! runic-commands — slash commands defined as markdown (`COMMAND.md`).
//!
//! A command is a reusable prompt template the user invokes as `/name args`
//! from whatever surface hosts the agent (REPL, HTTP, …). Like skills and
//! agents, commands are purely declarative — a directory per command under
//! some root, each holding a `COMMAND.md` with YAML frontmatter and a
//! markdown body:
//!
//! ```text
//! commands/
//! └── review/
//!     └── COMMAND.md
//! ```
//!
//! ```markdown
//! ---
//! name: review
//! description: review a diff with my preferences
//! ---
//! Review the following with an eye for unwrap() abuse and missing tests:
//!
//! $ARGUMENTS
//! ```
//!
//! Invoking `/review src/lib.rs` expands the body with `$ARGUMENTS` replaced
//! by `src/lib.rs` and sends the result to the agent as the user message.
//! If the body has no `$ARGUMENTS` placeholder and arguments were given,
//! they're appended on a new line so nothing the user typed is dropped.
//!
//! The frontmatter/body format and the registry's load behavior are
//! deliberately identical to `runic-skills` — same `---` delimiters, same
//! tolerance for Directory- vs File-listing backends, same hard error on a
//! malformed file with the path in the payload.

use runic_storage_backend::{EntryKind, StorageBackend};
use std::collections::HashMap;
use std::sync::Arc;

/// Placeholder in a command body that gets replaced with everything the
/// user typed after the command name.
pub const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CommandMeta {
    pub name: String,
    pub description: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("missing or malformed frontmatter (expected '---' delimeters)")]
    MissingFrontMatter,

    #[error("invalid YAML in frontmatter: {0}")]
    InvalidYaml(#[from] serde_yml::Error),
}

#[derive(Debug, Clone)]
pub struct Command {
    pub meta: CommandMeta,
    pub body: String,
    pub dir: String,
}

impl Command {
    pub fn parse(raw: &str) -> Result<Self, ParseError> {
        let rest = raw
            .strip_prefix("---\n")
            .ok_or(ParseError::MissingFrontMatter)?;

        let close = rest.find("\n---\n").ok_or(ParseError::MissingFrontMatter)?;

        let descriptor = &rest[..close];
        let body = &rest[close + "\n---\n".len()..];

        let meta: CommandMeta = serde_yml::from_str(descriptor)?;

        Ok(Command {
            meta,
            body: body.to_string(),
            dir: String::new(),
        })
    }

    /// Expand the body with the user's arguments. `$ARGUMENTS` occurrences
    /// are replaced; when the placeholder is absent but arguments were
    /// given, they're appended on their own line so user input is never
    /// silently dropped.
    pub fn expand(&self, args: &str) -> String {
        let args = args.trim();
        if self.body.contains(ARGUMENTS_PLACEHOLDER) {
            self.body.replace(ARGUMENTS_PLACEHOLDER, args)
        } else if args.is_empty() {
            self.body.clone()
        } else {
            format!("{}\n\n{args}", self.body.trim_end())
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("storage error: {0}")]
    Storage(#[from] runic_storage_backend::StorageError),

    #[error("failed to parse command at '{path}': {source}")]
    Parse {
        path: String,
        #[source]
        source: ParseError,
    },
}

/// Pure-data index of parsed commands — holds no storage reference after
/// loading, same contract as `SkillRegistry`.
#[derive(Debug, Clone, Default)]
pub struct CommandRegistry {
    commands: HashMap<String, Command>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan `root` in `storage`, parse every `{dir}/COMMAND.md` found, and
    /// return a populated registry. Directories without a `COMMAND.md` are
    /// silently skipped; a malformed `COMMAND.md` is a hard error carrying
    /// the offending path.
    pub async fn load(storage: Arc<dyn StorageBackend>, root: &str) -> Result<Self, LoadError> {
        let mut commands = HashMap::new();
        let entries = storage.list(root).await?;

        // Same dual-shape tolerance as SkillRegistry::load — hierarchical
        // backends list Directory entries, flat KV backends list full File
        // keys.
        for entry in &entries {
            let path = match entry.kind {
                EntryKind::Directory => format!("{}/COMMAND.md", entry.key),
                EntryKind::File if entry.key.ends_with("/COMMAND.md") => entry.key.clone(),
                _ => continue,
            };

            let raw = match storage.read_to_string(&path).await {
                Ok(r) => r,
                Err(_) => continue,
            };

            let mut command = Command::parse(&raw).map_err(|e| LoadError::Parse {
                path: path.clone(),
                source: e,
            })?;

            command.dir = std::path::Path::new(&path)
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            commands.insert(command.meta.name.clone(), command);
        }

        Ok(Self { commands })
    }

    /// Insert a command directly. Useful for tests.
    pub fn insert(&mut self, command: Command) {
        self.commands.insert(command.meta.name.clone(), command);
    }

    pub fn get(&self, name: &str) -> Option<&Command> {
        self.commands.get(name)
    }

    /// All commands sorted by name, for deterministic help listings.
    pub fn list(&self) -> Vec<&Command> {
        let mut out: Vec<&Command> = self.commands.values().collect();
        out.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
        out
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// Split a raw `/name the rest` invocation into `(name, args)`. Returns
/// `None` when the input isn't a slash invocation at all (doesn't start
/// with `/`, or has no name).
pub fn split_invocation(input: &str) -> Option<(&str, &str)> {
    let rest = input.strip_prefix('/')?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        return None;
    }
    match rest.split_once(char::is_whitespace) {
        Some((name, args)) => Some((name, args.trim())),
        None => Some((rest, "")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_storage_backend::MemoryBackend;

    fn command_md(name: &str, description: &str, body: &str) -> Vec<u8> {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}").into_bytes()
    }

    // ─── Parsing (delimiter strictness is covered exhaustively in
    //     runic-skills; the parser here is the same shape) ──────────────────

    #[test]
    fn parses_a_minimal_command() {
        let raw = "---\nname: review\ndescription: review a diff\n---\nReview this:\n\n$ARGUMENTS\n";
        let cmd = Command::parse(raw).expect("should parse");
        assert_eq!(cmd.meta.name, "review");
        assert!(cmd.body.contains("$ARGUMENTS"));
    }

    #[test]
    fn rejects_missing_frontmatter() {
        assert!(matches!(
            Command::parse("no frontmatter here").unwrap_err(),
            ParseError::MissingFrontMatter
        ));
    }

    #[test]
    fn rejects_missing_required_fields() {
        let raw = "---\nname: lonely\n---\nbody";
        assert!(matches!(
            Command::parse(raw).unwrap_err(),
            ParseError::InvalidYaml(_)
        ));
    }

    // ─── Expansion ──────────────────────────────────────────────────────────

    #[test]
    fn expand_replaces_arguments_placeholder() {
        let cmd = Command::parse("---\nname: r\ndescription: d\n---\nReview: $ARGUMENTS!").unwrap();
        assert_eq!(cmd.expand("src/lib.rs"), "Review: src/lib.rs!");
    }

    #[test]
    fn expand_replaces_every_placeholder_occurrence() {
        let cmd =
            Command::parse("---\nname: r\ndescription: d\n---\n$ARGUMENTS and $ARGUMENTS").unwrap();
        assert_eq!(cmd.expand("x"), "x and x");
    }

    #[test]
    fn expand_with_empty_args_blanks_the_placeholder() {
        let cmd = Command::parse("---\nname: r\ndescription: d\n---\nReview: $ARGUMENTS").unwrap();
        assert_eq!(cmd.expand("  "), "Review: ");
    }

    #[test]
    fn expand_appends_args_when_no_placeholder() {
        let cmd = Command::parse("---\nname: r\ndescription: d\n---\nDo the thing.\n").unwrap();
        assert_eq!(cmd.expand("with feeling"), "Do the thing.\n\nwith feeling");
    }

    #[test]
    fn expand_without_placeholder_or_args_returns_body_verbatim() {
        let cmd = Command::parse("---\nname: r\ndescription: d\n---\nDo the thing.\n").unwrap();
        assert_eq!(cmd.expand(""), "Do the thing.\n");
    }

    // ─── Invocation splitting ───────────────────────────────────────────────

    #[test]
    fn split_invocation_handles_name_and_args() {
        assert_eq!(split_invocation("/review src/lib.rs"), Some(("review", "src/lib.rs")));
        assert_eq!(split_invocation("/review"), Some(("review", "")));
        assert_eq!(split_invocation("/review   spaced   "), Some(("review", "spaced")));
    }

    #[test]
    fn split_invocation_rejects_non_commands() {
        assert_eq!(split_invocation("plain text"), None);
        assert_eq!(split_invocation("/"), None);
        assert_eq!(split_invocation("/ leading space"), None);
        assert_eq!(split_invocation(""), None);
    }

    // ─── Registry load ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn load_discovers_and_parses_commands() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write(
                "commands/review/COMMAND.md",
                &command_md("review", "review a diff", "Review:\n$ARGUMENTS"),
            )
            .await
            .unwrap();
        storage
            .write(
                "commands/standup/COMMAND.md",
                &command_md("standup", "summarize recent work", "Summarize."),
            )
            .await
            .unwrap();
        // Unrelated sibling must be ignored.
        storage
            .write("commands/notes/random.txt", b"unrelated")
            .await
            .unwrap();

        let reg = CommandRegistry::load(storage, "commands").await.unwrap();
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.get("review").unwrap().dir, "review");
        let names: Vec<&str> = reg.list().iter().map(|c| c.meta.name.as_str()).collect();
        assert_eq!(names, vec!["review", "standup"]);
    }

    #[tokio::test]
    async fn load_from_empty_root_yields_empty_registry() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        let reg = CommandRegistry::load(storage, "commands").await.unwrap();
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn load_errors_with_path_on_malformed_command() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage
            .write("commands/broken/COMMAND.md", b"no frontmatter")
            .await
            .unwrap();

        match CommandRegistry::load(storage, "commands").await.unwrap_err() {
            LoadError::Parse { path, .. } => assert_eq!(path, "commands/broken/COMMAND.md"),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }
}
