# runic-skills

Progressive-disclosure skills: each skill is a folder with a `SKILL.md`
(YAML frontmatter + Markdown body). The model sees only a compact **index**
(one line per skill); it loads a skill's full body on demand through a view
tool. Cheap context by default, full detail when needed.

This crate produces two values and stops there — what you do with them is
outside its scope:

- `prompt_section()` → a `String` (the index section)
- `skill_tool()` → an `Option<Arc<dyn Tool>>` (the loader tool)

## The whole API in one pass

```rust
use std::collections::HashMap;
use std::sync::Arc;
use runic_skills::{SkillSet, source};

let set = SkillSet::load(HashMap::from([
    ("core".to_string(), source::local("/srv/skills/core")),
    ("acme".to_string(), source::local("/srv/tenants/acme/skills")),
])).await
    .tag("playbooks")                                        // wrapper tag
    .intro("Consult the relevant playbook before acting:")   // text after the tag
    .tool_name("open_playbook")                              // rename the read_skill tool
    .tool_description("Open a playbook by id.");             // its description

let narrowed = set.scope_glob(&["core:*"]);                  // voice travels with it

let set = Arc::new(set);
let section = set.prompt_section();                          // String
let tool = set.skill_tool();                                  // Option<Arc<dyn Tool>>
```

What `section` contains:

```text
<playbooks>
Consult the relevant playbook before acting:
- core:deploy: ship a service to production safely
- acme:onboard: onboard a new ACME customer
</playbooks>
```

The `- id: description` lines come verbatim from your `SKILL.md` files and
are not configurable here — edit the files. Everything else in the section
and on the tool is configurable; untouched knobs keep their defaults (see
[customization.md](customization.md) for the defaults, quoted verbatim).

## Pages

- [skill-format.md](skill-format.md) — `SKILL.md` anatomy, folder layout,
  sanitization, exact limits.
- [loading.md](loading.md) — `SkillSource`, `local()`, the `s3` stub,
  namespaces, per-tenant maps, best-effort loading semantics.
- [customization.md](customization.md) — the four voice knobs and their
  defaults; the hand-rolled tier via `skills()`.
- [scoping.md](scoping.md) — `scope`, `scope_glob`, `merge`, and how the
  configured voice propagates.
- [read-skill.md](read-skill.md) — the read_skill tool: schema, body and sub-file
  reads, path-traversal protection, error cases.
- [examples/](examples/) — three complete example skills.
