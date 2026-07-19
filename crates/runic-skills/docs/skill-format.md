# SKILL.md format

A skill is a folder containing a `SKILL.md`, plus any sub-files the skill
wants to reference:

```text
deploy/
  SKILL.md
  checklist.md        (optional — readable via the view tool's `path` param)
  runbooks/
    rollback.md       (nested sub-files work too)
```

## SKILL.md anatomy

```markdown
---
name: deploy
description: ship a service to production safely
---
Full instructions in Markdown. This body is what the model receives when it
loads the skill — everything after the closing frontmatter fence, trimmed.
```

- **Frontmatter** is YAML between `---` fences. Recognized fields: `name`,
  `description`. Unknown fields are ignored.
- **`name`** — optional; falls back to the folder name if empty.
- **`description`** — the one-liner shown in the prompt index. Required
  after sanitization (an empty description drops the skill).
- **Body** — everything after the closing `---`, leading newline stripped,
  trimmed. Returned verbatim by the view tool.

## Sanitization and limits

Names and descriptions land in the system prompt, so they are hardened at
load:

- All whitespace runs (newlines, tabs) collapse to single spaces — a
  description cannot smuggle newlines or a fake closing tag into the index.
- Length caps (characters): `name` ≤ **64**, `description` ≤ **1024**,
  qualified `namespace:name` id ≤ **128**.
- At most **2000** skill folders are read per source; the rest are skipped
  with a warning.

## What loads and what drops

Loading is best-effort:

- A folder without a `SKILL.md` is silently skipped (it just isn't a skill).
- A `SKILL.md` that fails parsing or a limit is dropped with a
  `tracing::warn!` naming the entry and the reason.
- Dotfolders (`.git`, …) and symlinked directories are skipped at the
  source level.
- An unreadable source is skipped with a warning; the rest of the map still
  loads.

Nothing aborts the whole load; you always get a `SkillSet` of everything
that conformed.
