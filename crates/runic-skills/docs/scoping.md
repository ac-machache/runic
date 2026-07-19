# Scoping and merging

All three operations return a new `SkillSet`; the original is untouched.
Sources are pruned to the namespaces the surviving skills actually use, so
a narrowed set can still read its sub-files but holds nothing else.

## scope — exact ids

```rust
let narrowed = set.scope(&["core:deploy", "acme:onboard"]);
```

Keeps exactly the listed ids. Anything else — including typos — is silently
absent from the result.

## scope_glob — patterns

```rust
set.scope_glob(&["*"]);              // everything
set.scope_glob(&["core:*"]);         // one whole namespace
set.scope_glob(&["core:deploy"]);    // exact id
set.scope_glob(&["core:*", "acme:onboard"]);   // mix freely
```

Three pattern forms only: `*`, `namespace:*`, and exact ids. A **bare
namespace name is not a wildcard** — `"core"` matches nothing; write
`"core:*"`. Overlapping patterns do not duplicate skills. An empty pattern
list yields an empty set.

## Voice propagation

`scope` and `scope_glob` **preserve** the configured voice (`tag`, `intro`,
`tool_name`, `tool_description`). A narrowed set renders and names its tool
exactly like its parent — customize once, narrow freely.

## merge

```rust
let combined = SkillSet::merge([core_set, tenant_set]);   // impl IntoIterator<Item = Arc<SkillSet>>
```

- **Skills** deduplicate by id — the first occurrence wins.
- **Sources** deduplicate by namespace — the first occurrence wins.
- **Voice**: per knob, the first *configured* (non-default) value wins.
  Merging a default-voiced set with a customized one keeps the customized
  voice regardless of order relative to defaults.

To give a merged set a single deliberate voice, restate after merging —
the fluent methods work on any set:

```rust
let combined = SkillSet::merge([core_set, tenant_set])
    .intro("These are your playbooks and this tenant's own procedures:");
```

## Inspection

`get(id)`, `ids()`, `len()`, `is_empty()`, and `skills()` work on any set,
scoped or merged — useful for asserting exactly what a narrowed set
contains before you render anything.
