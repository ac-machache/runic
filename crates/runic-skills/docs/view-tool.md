# The view tool

`set.view_tool()` returns the tool the model calls to load skill content —
`Option<Arc<dyn Tool>>`, and `None` when the set is empty (no skills, no
tool, no section: the feature disappears cleanly).

## Identity

- **Name**: `skill_view` by default; whatever you set with `.tool_name(...)`.
- **Description**: ``Load a skill's full instructions by `name`, or a file
  inside the skill's folder by also passing a relative `path`.`` by default;
  whatever you set with `.tool_description(...)`.

The tool captures the set's configuration at the moment `view_tool()` is
called.

## Parameters

```json
{
  "type": "object",
  "properties": {
    "name": { "type": "string", "description": "Skill id from the index (e.g. `core:deploy`)." },
    "path": { "type": "string", "description": "Optional file path relative to the skill folder." }
  },
  "required": ["name"]
}
```

## Behavior

- `{ "name": "core:deploy" }` → the skill's full `SKILL.md` body, verbatim.
- `{ "name": "core:deploy", "path": "checklist.md" }` → that file's
  contents, read **through the skill's own source** — an S3 skill's
  sub-file is fetched from S3; the tool never knows where the skill lives.

## Errors (returned in-band as tool errors, never panics)

- Missing `name` → `` `<tool name>` requires `name` ``.
- Unknown id → `unknown skill '<name>'` — ids are the *qualified* form from
  the index; a bare `deploy` is unknown if the skill lives at `core:deploy`.
- Bad `path` → rejected before any I/O when it is absolute, contains `..`,
  or has empty segments; local reads additionally canonicalize and verify
  the resolved file stayed under the source root, so symlinks cannot
  escape either.
- Unreadable sub-file → the source's error message, in-band.
