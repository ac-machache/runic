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

When in doubt, leave it out.
