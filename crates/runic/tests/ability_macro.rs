use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::{Ability, AbilityDraft, ActivationPolicy, BuildCtx};
use runic::composer::Agent;
use runic::{Llm, ability};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, StopReason, TokenUsage};

struct ScriptedProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
        })
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Parse("exhausted".into()))
    }
}

fn text(content: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: content.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage::default(),
    }
}

struct RefundTool;

#[async_trait]
impl Tool for RefundTool {
    fn name(&self) -> &str {
        "refund"
    }
    fn description(&self) -> &str {
        "issues a refund"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("refunded"))
    }
}

#[ability]
struct Bare;

impl Bare {
    async fn ability(
        &self,
        draft: AbilityDraft,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<AbilityDraft> {
        Ok(draft)
    }
}

#[ability(activation = deferred, id = "billing", description = "invoices and refunds")]
struct Billing {
    tool_count: usize,
}

impl Billing {
    fn banner(&self) -> String {
        format!("Billing rules ({} tools).", self.tool_count)
    }

    async fn ability(
        &self,
        draft: AbilityDraft,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<AbilityDraft> {
        Ok(draft.prompt(self.banner()).tool(RefundTool))
    }
}

#[ability(activation = eager, name = "logging")]
struct Logging;

impl Logging {
    async fn ability(
        &self,
        draft: AbilityDraft,
        _ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<AbilityDraft> {
        Ok(draft)
    }
}

#[ability(activation = eager, name = "tenant-kit")]
struct TenantKit;

impl TenantKit {
    async fn ability(
        &self,
        draft: AbilityDraft,
        ctx: &BuildCtx<'_>,
    ) -> anyhow::Result<AbilityDraft> {
        Ok(draft
            .prompt(format!("Tenant: {}.", ctx.tenant))
            .with(ability("nested").prompt("NESTED-SECTION")))
    }
}

#[test]
fn deferred_attrs_become_the_descriptor_and_the_name() {
    let billing = Billing { tool_count: 1 };
    assert_eq!(billing.name(), "billing");

    let descriptor = billing.descriptor();
    assert_eq!(descriptor.activation, ActivationPolicy::Deferred);
    assert_eq!(descriptor.id.as_deref(), Some("billing"));
    assert_eq!(
        descriptor.description.as_deref(),
        Some("invoices and refunds")
    );
}

#[test]
fn a_bare_ability_keeps_the_trait_defaults() {
    let descriptor = Bare.descriptor();
    assert_eq!(descriptor.activation, ActivationPolicy::Eager);
    assert!(descriptor.id.is_none());
    // no `name`/`id` given, so the trait's type_name default stands
    assert!(Bare.name().contains("Bare"));
}

#[test]
fn activation_eager_can_be_stated_explicitly() {
    assert_eq!(Logging.descriptor().activation, ActivationPolicy::Eager);
    assert_eq!(Logging.name(), "logging");
}

#[test]
fn the_impl_block_is_left_untouched() {
    assert_eq!(
        Billing { tool_count: 2 }.banner(),
        "Billing rules (2 tools)."
    );
}

#[tokio::test]
async fn the_draft_body_sees_ctx_and_can_nest_abilities() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let agent = Agent::new(Llm::new(provider, "test-model").instructions("core"))
        .with(TenantKit)
        .build("acme", "s1")
        .await
        .unwrap();

    let system = &agent.state().system_prompt;
    assert!(system.contains("Tenant: acme."), "{system}");
    assert!(system.contains("NESTED-SECTION"), "{system}");
}

#[tokio::test]
async fn the_generated_ability_composes_and_stays_hidden_until_loaded() {
    let provider = ScriptedProvider::new(vec![text("done")]);
    let agent = Agent::new(Llm::new(provider, "test-model").instructions("core"))
        .with(Billing { tool_count: 1 })
        .build("alice", "s1")
        .await
        .unwrap();

    let system = &agent.state().system_prompt;
    assert!(
        system.contains("- billing: invoices and refunds"),
        "the description advertises it in the catalog: {system}"
    );
    assert!(
        !system.contains("Billing rules"),
        "the prompt fragment stays withheld until load_ability: {system}"
    );
}
