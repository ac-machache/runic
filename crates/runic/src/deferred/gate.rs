use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use runic_tool::{Tool, ToolContext, ToolResult};

#[derive(Clone, Default)]
pub(crate) struct LoadedAbilities(Arc<RwLock<HashSet<String>>>);

impl LoadedAbilities {
    pub(crate) fn seeded(ids: &HashSet<String>) -> Self {
        Self(Arc::new(RwLock::new(ids.clone())))
    }

    pub(crate) fn mark(&self, id: &str) {
        self.0.write().unwrap().insert(id.to_string());
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.0.read().unwrap().contains(id)
    }
}

pub(crate) fn skill_subjects(args: &serde_json::Value) -> Vec<String> {
    args.get("name")
        .and_then(|value| value.as_str())
        .map(|name| vec![name.to_string()])
        .unwrap_or_default()
}

pub(crate) fn delegate_subjects(args: &serde_json::Value) -> Vec<String> {
    let mut subjects = Vec::new();
    if let Some(agent) = args.get("agent").and_then(|value| value.as_str()) {
        subjects.push(agent.to_string());
    }
    if let Some(parallel) = args.get("parallel").and_then(|value| value.as_array()) {
        subjects.extend(
            parallel
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string)),
        );
    }
    subjects
}

pub(crate) struct GatedTool {
    inner: Arc<dyn Tool>,
    loaded: LoadedAbilities,
    owners: HashMap<String, String>,
    kind: &'static str,
    subjects: fn(&serde_json::Value) -> Vec<String>,
}

impl GatedTool {
    pub(crate) fn new(
        inner: Arc<dyn Tool>,
        loaded: LoadedAbilities,
        owners: HashMap<String, String>,
        kind: &'static str,
        subjects: fn(&serde_json::Value) -> Vec<String>,
    ) -> Self {
        Self {
            inner,
            loaded,
            owners,
            kind,
            subjects,
        }
    }

    fn blocked(&self, args: &serde_json::Value) -> Option<(String, String)> {
        (self.subjects)(args).into_iter().find_map(|subject| {
            let ability = self.owners.get(&subject)?;
            if self.loaded.contains(ability) {
                None
            } else {
                Some((subject, ability.clone()))
            }
        })
    }
}

#[async_trait]
impl Tool for GatedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    fn parallelizable(&self) -> bool {
        self.inner.parallelizable()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        if let Some((subject, ability)) = self.blocked(&args) {
            return Ok(ToolResult::error(format!(
                "{} `{subject}` is not available yet. Load ability `{ability}` with the `load_ability` tool first.",
                self.kind
            )));
        }
        self.inner.execute(args, ctx).await
    }
}
