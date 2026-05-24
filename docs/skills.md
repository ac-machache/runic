# Skills

A **skill** is a markdown file the model can load on demand. Same shape
as Claude Code's skills: drop a `SKILL.md` into `~/.runic/skills/{name}/`
and the binary picks it up at startup.

The model sees a compact one-line entry for each skill in its system
prompt. When a skill becomes relevant, the model calls `skill_view(name)`
to load the full body.

## File format

```
~/.runic/skills/code-review/SKILL.md
```

```yaml
---
name: code-review
description: Review a diff for bugs, style, and security
---
# Code Review

When given a diff, walk through each change and report:
- Bugs (off-by-one, null-deref, race conditions, ...)
- Style issues (naming, structure, idiomaticity)
- Security concerns (injection, secrets in code, etc.)

Be terse. One bullet per issue. Mention the file + line.
```

Required frontmatter: `name`, `description`. Unknown fields are silently
ignored — Claude Code's `allowed-tools`, `model`, etc. won't break
parsing if you paste their files in.

## What the model sees

The `SkillsIndexLayer` injects a block like this every turn:

```
<available-skills>
You have access to these skills — each one is a focused workflow you can invoke.
To load a skill's full instructions, call the `skill_view` tool with the skill's `name`.
For supporting files inside a skill (e.g. references, templates), call `skill_view`
with both `name` and a `path` relative to the skill's directory.

- code-review: Review a diff for bugs, style, and security
- greet: Greet the user warmly by name
- optimize: Find performance hotspots and propose improvements
</available-skills>
```

Only `name` + `description` are in the prompt — the full body loads
lazily via the `skill_view` tool. 50 skills × 1 line each = ~50 lines
of context, regardless of skill body size.

## Sub-files

A skill can ship supporting material in its directory:

```
~/.runic/skills/code-review/
  SKILL.md
  references/
    style-guide.md
    common-bugs.md
  templates/
    review-pr.md
```

The model loads sub-files via `skill_view(name, path)`:

```
skill_view({ "name": "code-review", "path": "references/style-guide.md" })
```

Sub-file paths are relative to the skill directory. The tool rejects
`..`, absolute paths, and empty segments — the model cannot escape the
skill's directory.

## Programmatic access

```rust
use runic_skills::{SkillRegistry, SkillsIndexLayer, SkillViewTool};
use runic_storage_backend::LocalFsBackend;
use std::sync::Arc;

let storage = Arc::new(LocalFsBackend::new("~/.runic"));
let registry = Arc::new(SkillRegistry::load(storage.clone(), "skills").await?);

// Layer goes into the CompositeEngine
let layer = SkillsIndexLayer::new(registry.clone());

// Tool goes into the agent's ToolRegistry
let tool = SkillViewTool::new(registry.clone(), storage.clone(), "skills");
```

`SkillRegistry` is pure data after `load()` — no retained storage handle.
The tool keeps its own storage handle for sub-file reads.

## Inspecting a registry

```rust
println!("Loaded {} skills", registry.len());
for skill in registry.list() {
    println!("  {} — {}", skill.meta.name, skill.meta.description);
}

if let Some(skill) = registry.get("code-review") {
    println!("Body: {}", skill.body);
    println!("Directory: {}", skill.dir);
}
```

## Storage tolerance

`SkillRegistry::load` works against both backend semantics:

- **Hierarchical** (`LocalFsBackend`): `list("skills")` returns
  Directory entries; the loader reads `{dir}/SKILL.md` from each.
- **Flat KV** (`MemoryBackend`, S3-style): `list("skills")` returns File
  entries with full keys like `skills/code-review/SKILL.md`; the loader
  recognizes the `/SKILL.md` suffix.

In practice this means you can test against `MemoryBackend` in unit
tests and deploy against `LocalFsBackend` (or an S3 backend later)
without touching the registry code.

## Skill from a plugin

Skills shipped inside a plugin live under:

```
~/.runic/plugins/{plugin-name}/skills/{skill-name}/SKILL.md
```

The plugin manager loads them and merges into the same flat registry
the agent sees. See [plugins.md](./plugins.md).

## When NOT to use a skill

Skills are good for:
- **Workflows the model executes** (review, refactor, document, debug)
- **Domain-specific instructions** (how to write a PR, how to format a stand-up)
- **Heavy text content** the model only sometimes needs

Skills are NOT for:
- **Persona** — use `SOUL.md` (`PersonaLayer`) instead
- **User facts** — use `memory/USER.md` (`UserFactsLayer`) instead
- **Tools the model invokes directly** — write a real `Tool` instead

## Testing

The crate ships 50 tests covering parser edge cases (malformed
delimiters, multi-line frontmatter, body with horizontal rules),
registry behavior, layer rendering, and the path-traversal guard on
the view tool. Use them as a reference if you're hacking on the
skills crate.
