# Slash commands (`runic-commands`)

A command is a reusable prompt template invoked as `/name args` from the
REPL (or any other surface that chooses to wire the registry in). Like
skills and agents, commands are purely declarative markdown — no Rust
required to add one.

## Layout

```
~/.runic/commands/
└── review/
    └── COMMAND.md
```

## Format

```markdown
---
name: review
description: review a diff with my preferences
---
Review the following with an eye for unwrap() abuse and missing tests:

$ARGUMENTS
```

Frontmatter is YAML between `---` delimiters (same strict rules as
`SKILL.md` — see [skills.md](./skills.md)). `name` and `description` are
required; unknown fields are ignored.

## Expansion

`/review src/lib.rs` expands the body and sends the result to the agent as
the user message — the model never sees the slash invocation itself.

- Every `$ARGUMENTS` occurrence is replaced with the text after the command
  name (trimmed).
- If the body has no `$ARGUMENTS` and arguments were given, they're
  appended on their own line, so user input is never dropped.
- Unknown `/whatever` input is intercepted by the REPL and answered with
  the list of available commands instead of being sent to the model.

Builtin REPL commands (`/state`, `/dump`, `/quit`, `/exit`) take
precedence over user-defined commands with the same name.

## API

```rust
use runic_commands::{CommandRegistry, split_invocation};

let registry = CommandRegistry::load(storage.clone(), "commands").await?;
if let Some((name, args)) = split_invocation(user_input) {
    if let Some(cmd) = registry.get(name) {
        let prompt = cmd.expand(args);
        // hand `prompt` to the agent
    }
}
```

The registry is pure data after `load` — it keeps no storage reference,
same contract as `SkillRegistry`.
