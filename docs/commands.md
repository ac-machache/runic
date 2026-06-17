# Slash commands (`runic-commands`)

A command is a reusable prompt template invoked as `/name args` from any
surface that wires the registry in. In the reference server it's wired as
a `CommandExpansionEngine` context layer, which expands a matching
invocation before the message reaches the model. Like skills and agents,
commands are purely declarative markdown — no Rust required to add one.

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
- Input that isn't a known command passes through unchanged to the model.

Wiring is up to the surface: the reference server expands commands via a
`CommandExpansionEngine`; a custom surface can call the registry directly
(see the API below).

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
