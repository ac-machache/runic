//! `runic-skills` — progressive-disclosure skills (the Anthropic Agent Skills
//! pattern, runic's design), loaded from any read-only [`SkillSource`].
//!
//! A skill is a `SKILL.md`: YAML frontmatter (`name`, `description`) + a
//! Markdown body of full instructions. The model sees only a compact **index**
//! (id + one-line description) — [`SkillSet::prompt_section`] — and calls the
//! `skill_view` tool ([`SkillSet::view_tool`]) to load a skill's full body, or
//! a sub-file in its folder, on demand. Cheap context, lazy load.
//!
//! Skills are **static, read-only config**. A [`SkillSet`] is loaded from a map
//! of `namespace -> SkillSource`, so what one agent sees is just which map you
//! hand it — that's the whole per-tenant story, with no global state:
//!
//! ```ignore
//! use runic_skills::{SkillSet, source};
//! let skills = SkillSet::load(std::collections::HashMap::from([
//!     ("core".into(), source::local("/srv/skills/core")),     // shared
//!     ("acme".into(), source::s3("acme-bucket", "skills/")),  // tenant (s3 feature)
//! ])).await;
//! ```

mod security;
mod set;
pub mod source;
mod view;

use std::sync::Arc;

use runic_tool::Tool;

pub use set::{Skill, SkillSet};
pub use source::SkillSource;
pub use view::SkillViewTool;

impl SkillSet {
    /// The `skill_view` tool for this set, or `None` if it's empty.
    pub fn view_tool(self: &Arc<Self>) -> Option<Arc<dyn Tool>> {
        if self.is_empty() {
            None
        } else {
            Some(Arc::new(SkillViewTool::new(self.clone())) as Arc<dyn Tool>)
        }
    }
}
