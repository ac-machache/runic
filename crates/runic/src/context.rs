/// Where a fragment sits relative to the prompt-cache boundary. Stable
/// fragments form the cacheable prefix; volatile ones (per-turn freshness)
/// go after it so they don't invalidate the cache.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Stable,
    Volatile,
}

/// One section of the system prompt. The body is the section's own
/// (already self-wrapped) text; `name` is for debugging/tests.
struct Fragment {
    #[allow(dead_code)]
    name: &'static str,
    body: String,
    layer: Layer,
}

/// Composes the agent's system prompt from each configured part's section.
/// Stable fragments render before volatile ones, keeping the cacheable prefix
/// byte-stable (see [`Context::render_parts`]). Empty sections are dropped.
#[derive(Default)]
pub struct Context {
    fragments: Vec<Fragment>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn instructions(&mut self, text: &str) -> &mut Self {
        self.push("instructions", text, Layer::Stable);
        self
    }

    pub(crate) fn fragment(&mut self, layer: Layer, text: impl Into<String>) {
        self.push("ability", &text.into(), layer);
    }

    fn push(&mut self, name: &'static str, body: &str, layer: Layer) {
        if !body.trim().is_empty() {
            self.fragments.push(Fragment {
                name,
                body: body.to_string(),
                layer,
            });
        }
    }

    fn join<'a>(fragments: impl Iterator<Item = &'a Fragment>) -> String {
        fragments
            .map(|f| f.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The cacheable stable prefix and the volatile tail, separately. The
    /// boundary between them is the prompt-cache seam: a future provider layer
    /// can mark `cache_control` at the end of `stable`. Today the agent just
    /// consumes [`Context::render`].
    pub fn render_parts(&self) -> (String, String) {
        let stable = Self::join(self.fragments.iter().filter(|f| f.layer == Layer::Stable));
        let volatile = Self::join(self.fragments.iter().filter(|f| f.layer == Layer::Volatile));
        (stable, volatile)
    }

    /// The full system prompt: stable fragments first, then volatile.
    pub fn render(&self) -> String {
        let (stable, volatile) = self.render_parts();
        match (stable.is_empty(), volatile.is_empty()) {
            (_, true) => stable,
            (true, _) => volatile,
            _ => format!("{stable}\n\n{volatile}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_renders_before_volatile_and_drops_empty() {
        let mut ctx = Context::new();
        ctx.push("a", "alpha", Layer::Stable);
        ctx.push("z", "zeta", Layer::Volatile);
        ctx.push("b", "beta", Layer::Stable);
        ctx.push("empty", "   ", Layer::Stable); // dropped

        assert_eq!(ctx.render(), "alpha\n\nbeta\n\nzeta");
        let (stable, volatile) = ctx.render_parts();
        assert_eq!(stable, "alpha\n\nbeta");
        assert_eq!(volatile, "zeta");
    }

    #[test]
    fn all_stable_has_empty_volatile_tail() {
        let mut ctx = Context::new();
        ctx.instructions("you are helpful");
        let (stable, volatile) = ctx.render_parts();
        assert_eq!(stable, "you are helpful");
        assert!(volatile.is_empty());
    }
}
