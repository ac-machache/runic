# Plugins

A **plugin** is a directory that bundles skills and/or markdown
sub-agents under one logical name. Same shape as Claude Code's
plugins: a folder you drop in, things get registered.

## Layout

```
~/.runic/plugins/
  code-review/
    skills/
      review-diff/SKILL.md
      review-style/SKILL.md
    agents/
      reviewer/AGENT.md
  ops-toolkit/
    skills/
      deploy/SKILL.md
      rollback/SKILL.md
```

A plugin can ship:
- `skills/` — same format as top-level skills (see [skills.md](./skills.md))
- `agents/` — same format as top-level agents (see [agents.md](./agents.md))
- Both, either, or neither (an empty plugin is fine but pointless)

## How they're loaded

At startup the binary:

1. Loads any top-level `~/.runic/skills/` and `~/.runic/agents/` (these
   are NOT inside any plugin).
2. Scans `~/.runic/plugins/{name}/` for each plugin directory.
3. For each plugin, parses its `skills/` and `agents/` subdirs into
   per-plugin registries.
4. **Merges** every plugin's skills/agents into the flat registries
   from step 1.

The model sees one combined list of skills and one combined list of
agents — it doesn't know (or care) which came from a plugin.

## Collision policy

If two sources (top-level + plugin, or two plugins) declare a skill
with the same `name`, the **last-loaded one wins**. The plugin manager
loads plugins in alphabetical order, AFTER top-level. So:

- Top-level `skills/foo` is loaded first
- Plugin `alpha`'s `foo` overrides it
- Plugin `beta`'s `foo` overrides `alpha`

A warning is logged when this happens:

```
WARN duplicate skill name across plugins — later plugin wins
   skill=foo previous_plugin=alpha new_plugin=beta
```

If you don't want plugins overriding your top-level entries, give them
distinct names.

## Programmatic access

```rust
use runic_plugins::PluginManager;
use runic_storage_backend::LocalFsBackend;
use std::sync::Arc;

let storage = Arc::new(LocalFsBackend::new("~/.runic"));
let plugins = PluginManager::load(storage, "plugins").await?;

println!("{} plugin(s) loaded", plugins.len());
for plugin in plugins.plugins() {
    println!("  {} ({} skills, {} agents)",
        plugin.name, plugin.skills.len(), plugin.agents.len());
}

// Aggregate views, ready to merge into the agent's main registries
let skill_registry = plugins.aggregate_skills();
let agent_registry = plugins.aggregate_agents();
```

`aggregate_skills` and `aggregate_agents` produce merged registries
with the collision policy described above. They DO NOT include the
top-level skills/agents — the binary merges those manually:

```rust
let mut skills = SkillRegistry::load(storage.clone(), "skills").await?;
for s in plugins.aggregate_skills().list() {
    skills.insert(s.clone());
}
```

## Per-plugin introspection

Each plugin's contributions are kept separately in `PluginManager` so
you can ask "which plugin shipped this skill?":

```rust
for plugin in plugins.plugins() {
    if plugin.skills.get("foo").is_some() {
        println!("'foo' comes from plugin '{}'", plugin.name);
    }
}
```

## Error isolation

If one plugin has a malformed `SKILL.md`, the load returns a
`LoadError::Skills { plugin: "name", source: ... }` that names the
offending plugin. Other plugins are NOT loaded in this case — `load`
returns at the first error. If you want best-effort loading (load
what you can, skip what fails), wrap the call yourself.

## A no-op plugin (just a README)

A plugin directory without `skills/` or `agents/` still registers as
an empty plugin. No errors. Useful for placeholder dirs or
documentation-only plugins:

```
~/.runic/plugins/notes/
  README.md
```

`PluginManager::load` will return a `Plugin { name: "notes", skills: empty, agents: empty }`.

## Compatibility with Claude Code plugins

Claude Code plugins typically look like:

```
plugin-name/
  plugin.json        ← metadata (we don't read this yet)
  skills/...
  agents/...
  commands/...       ← we don't support these yet (see roadmap)
  hooks.json         ← we don't support these yet
```

The `skills/` and `agents/` portions of a Claude Code plugin should
drop into `~/.runic/plugins/` unchanged. The `commands/` and
`hooks.json` portions are ignored for now.

## Storage tolerance

`PluginManager::load` works against both backend types (hierarchical
FS and flat KV). The discovery logic in `discover_plugin_names`
extracts the plugin name from either:

- Directory entries (LocalFs case)
- File entries with paths like `plugins/code-review/skills/.../SKILL.md`
  (Memory backend case)

You can test against `MemoryBackend` and deploy against `LocalFsBackend`
identically.

## Testing

`crates/runic-plugins/src/lib.rs` ships 9 tests covering multi-plugin
discovery, aggregation, duplicate-name handling, empty plugins, and
the error-isolation case. Use them as a reference.
