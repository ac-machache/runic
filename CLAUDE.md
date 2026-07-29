# runic — conventions

## Comments: sparse, not decorative

Code should read on its own. Don't narrate it.

- **Don't comment what the code already says.** No `// increment i`, no doc line
  on every struct field or function that just restates the name.
- **Comment the non-obvious only:** a *why* (a tradeoff, an invariant, a
  workaround), or a subtle *gotcha* the next reader can't infer from the code.
- Match the density of the surrounding code; if a module is comment-light, stay
  light.
- Prefer a clear name or a small refactor over a comment that explains a bad one.
- No section-divider banners or restating-the-obvious doc comments just to fill
  space.
- Removing an existing comment that adds nothing is an improvement, not a risk.

When in doubt, leave it out.

## Naming

- **No one-letter variable names.** Name every binding after its role
  (`bundle`, `catalog`, `hook`, `entry`) — including closure params and short
  scopes. Existing one-letter names in code being touched get upgraded.

## Module layout

- **`mod.rs` files contain ONLY `mod` declarations and `pub use` re-exports.**
  No types, no functions, no logic. Content lives in named sibling files.
- **Organize by domain, one module per concern.** Never pile a new feature into
  an unrelated existing module; a new domain gets a new module.
- In the `runic` umbrella crate: `ability/` (the ability *model*), `composer/`
  (the build pipeline), `deferred/` (activation machinery), `models.rs`
  (provider-string inference), `context.rs` (prompt layering), `builtin/`
  (the shipped capabilities — one file per domain, each holding its `Tool`
  impls and any ability that bundles them; there is no separate `runic-tools`
  crate). `lib.rs` re-exports the hot path (`Composer`, `Compose`, `ability`,
  `ComposeError`).
- **An ability is a bundle, not a wrapper.** `Agent` takes tools, hooks,
  skills and subagents directly; reach for an ability when several parts ship
  as a unit, or when it needs an id, a description, or deferred activation.
  Never write `ability(id).tool(x)` just to attach one tool.
