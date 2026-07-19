# Loading: sources and namespaces

## SkillSource

A source is any read-only place skills live. The whole abstraction is two
methods:

```rust
#[async_trait]
pub trait SkillSource: Send + Sync {
    async fn entries(&self) -> anyhow::Result<Vec<String>>;   // skill folder names, one level
    async fn read(&self, rel: &str) -> anyhow::Result<String>; // a file within the source
}
```

Implementations must keep `rel` inside their root. Two ship with the crate:

- **`source::local(dir)`** — a local directory over `tokio::fs`. A missing
  directory yields an empty source with a warning, not an error. Reads are
  guarded twice: `rel` is validated (no absolute paths, no `..`, no empty
  segments), then the resolved path is canonicalized and must stay under
  the canonicalized root — symlinks cannot escape.
- **`source::s3(bucket, prefix)`** — behind the `s3` feature flag; currently
  an `unimplemented!` extension point that proves the shape without pulling
  the AWS SDK into the default build.

A custom source is a small impl. In-memory, for tests or embedded skills:

```rust
struct MapSource { files: HashMap<String, String> }   // "deploy/SKILL.md" -> contents

#[async_trait]
impl SkillSource for MapSource {
    async fn entries(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.files.keys()
            .filter_map(|k| k.split_once('/').map(|(dir, _)| dir.to_string()))
            .collect::<std::collections::HashSet<_>>()
            .into_iter().collect())
    }
    async fn read(&self, rel: &str) -> anyhow::Result<String> {
        self.files.get(rel).cloned().ok_or_else(|| anyhow::anyhow!("not found: {rel}"))
    }
}
```

## Namespaces

`SkillSet::load` takes a map of `namespace -> source`:

```rust
let set = SkillSet::load(HashMap::from([
    ("core".to_string(), source::local("/srv/skills/core")),
    ("acme".to_string(), source::local("/srv/tenants/acme/skills")),
])).await;
```

- The **map key is the namespace**; a skill's id is `"namespace:name"`
  (`core:deploy`). An **empty namespace** gives bare ids (`deploy`).
- The same folder can be mounted under different namespaces for different
  sets; different tenants get entirely different maps. There is no global
  state — which map you load *is* the per-tenant story.
- Sub-file reads go through the skill's own source, so a skill loaded from
  S3 reads its sub-files from S3.

## Convenience

```rust
let set = SkillSet::load_dir("core", "/srv/skills/core").await;
```

is exactly `load` with a single `local` entry.

`load` never fails — see [skill-format.md](skill-format.md#what-loads-and-what-drops)
for the best-effort semantics.
