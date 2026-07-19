# Customization: the voice knobs

Everything model-visible that the crate itself authors is one of four
strings, each overridable with a fluent method. Untouched knobs keep their
defaults. The skill lines themselves (`- id: description`) always come from
your `SKILL.md` files and have no knob.

| Method | Controls | Default |
|---|---|---|
| `.tag(text)` | the section's wrapper tag | `available-skills` |
| `.intro(text)` | the text between the tag and the skill lines | ``Each skill is a focused workflow. To load a skill's full instructions call `skill_view` with its `name`; for a file inside the skill pass `name` + a relative `path`.`` |
| `.tool_name(text)` | the view tool's name | `skill_view` |
| `.tool_description(text)` | the view tool's description | ``Load a skill's full instructions by `name`, or a file inside the skill's folder by also passing a relative `path`.`` |

One interaction to know: the **default intro interpolates the configured
tool name**. Rename the tool without writing an intro and the default
sentence says ``call `open_playbook` with its `name` `` — the two can never
drift apart. A custom intro is used verbatim.

## Default output

```rust
let set = SkillSet::load_dir("core", "/srv/skills/core").await;
set.prompt_section();
```

```text
<available-skills>
Each skill is a focused workflow. To load a skill's full instructions call `skill_view` with its `name`; for a file inside the skill pass `name` + a relative `path`.
- core:deploy: ship a service to production safely
</available-skills>
```

## Customized output

```rust
let set = SkillSet::load_dir("core", "/srv/skills/core").await
    .tag("playbooks")
    .intro("Consult the relevant playbook before acting:")
    .tool_name("open_playbook")
    .tool_description("Open a playbook by id.");
set.prompt_section();
```

```text
<playbooks>
Consult the relevant playbook before acting:
- core:deploy: ship a service to production safely
</playbooks>
```

`view_tool()` now yields a tool named `open_playbook` with your
description; its parameters and behavior are unchanged.

## The hand-rolled tier

For a layout the knobs cannot express, skip `prompt_section()` entirely:
`skills()` exposes the loaded data (`Skill { namespace, name, description,
body }` plus `id()`), and you write the string yourself. `view_tool()` still
works — layout and loading are independent.

```rust
let mine: String = set.skills().iter()
    .map(|s| format!("* {} — {}", s.id(), s.description))
    .collect::<Vec<_>>()
    .join("\n");
```
