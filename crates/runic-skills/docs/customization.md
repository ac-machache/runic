# Customization: the voice knobs

Everything model-visible that the crate itself authors is one of two
strings, each overridable with a fluent method. Untouched knobs keep their
defaults. The skill lines themselves (`- id: description`) always come from
your `SKILL.md` files and have no knob.

| Method | Controls | Default |
|---|---|---|
| `.tag(text)` | the section's wrapper tag | `available-skills` |
| `.intro(text)` | the text between the tag and the skill lines | ``Each skill is a focused workflow. To read a skill's full instructions call `read_skill` with its `name`; for a file inside the skill pass `name` + a relative `path`.`` |

The tool's own name and description are **not** knobs — see
[read-skill.md](read-skill.md). They are fixed so that the name the default
intro prints is always the name the tool actually registers under. A custom
intro is used verbatim, so if you write one, name `read_skill` in it.

## Default output

```rust
let set = SkillSet::load_dir("core", "/srv/skills/core").await;
set.prompt_section();
```

```text
<available-skills>
Each skill is a focused workflow. To read a skill's full instructions call `read_skill` with its `name`; for a file inside the skill pass `name` + a relative `path`.
- core:deploy: ship a service to production safely
</available-skills>
```

## Customized output

```rust
let set = SkillSet::load_dir("core", "/srv/skills/core").await
    .tag("playbooks")
    .intro("Consult the relevant playbook before acting; read one with `read_skill`.");
set.prompt_section();
```

```text
<playbooks>
Consult the relevant playbook before acting; read one with `read_skill`.
- core:deploy: ship a service to production safely
</playbooks>
```

## The hand-rolled tier

For a layout the knobs cannot express, skip `prompt_section()` entirely:
`skills()` exposes the loaded data (`Skill { namespace, name, description,
body }` plus `id()`), and you write the string yourself. `skill_tool()` still
works — layout and loading are independent.

```rust
let mine: String = set.skills().iter()
    .map(|s| format!("* {} — {}", s.id(), s.description))
    .collect::<Vec<_>>()
    .join("\n");
```
