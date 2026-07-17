use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use runic::ability::ability;
use runic::composer::Composer;
use runic_agent::Agent;
use runic_hook::{HookOutcome, WriteHook};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{AgentFactory, BoxedAgentFactory, single_agent};
use runic_skills::SkillSet;
use runic_state::AgentState;
use runic_subagent::{SubagentBuilder, SubagentReq};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, Message, MessageContent, Role, StopReason, TokenUsage, ToolCall};

const ECHO_ABILITY: &str = "echo-pack";
const DOCS_ABILITY: &str = "docs-pack";
const DOCS_SKILL_ID: &str = "docs:task";
const DOCS_WORKER: &str = "docs-worker";

pub fn dummy_agents(real_mistral: bool) -> HashMap<String, BoxedAgentFactory> {
    single_agent("main", Arc::new(DummyFactory { real_mistral }))
}

pub struct DummyFactory {
    pub real_mistral: bool,
}

#[async_trait]
impl AgentFactory for DummyFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> anyhow::Result<Agent> {
        let provider: Arc<dyn Provider> = if self.real_mistral {
            let key = std::env::var("MISTRAL_API_KEY").unwrap_or_default();
            Arc::new(runic_provider::mistral::MistralDriver::new(key))
        } else {
            Arc::new(ScriptedProvider)
        };
        let model = std::env::var("RUNIC_MODEL").unwrap_or_else(|_| "mistral-medium-latest".into());
        let agent = Composer::new(provider, model)
            .instructions("e2e harness agent")
            .with(
                ability("core")
                    .tool(AddTool)
                    .tool(SlowTool)
                    .tool(FailTool)
                    .tool(runic::tools::AskUserTool)
                    .hook(MarkerHook),
            )
            .with(
                ability(ECHO_ABILITY)
                    .describe("echoes text back verbatim")
                    .deferred()
                    .tool(EchoTool),
            )
            .with(
                ability(DOCS_ABILITY)
                    .describe("a task skill and a worker subagent")
                    .deferred()
                    .skills(docs_skill().await)
                    .subagent(docs_worker()),
            )
            .subagent_builder(Arc::new(ChildBuilder))
            .build(tenant, session_id)
            .await?;
        Ok(agent)
    }
}

async fn docs_skill() -> Arc<SkillSet> {
    let dir = tempfile::tempdir().expect("tempdir");
    let skill_dir = dir.path().join("task");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: task\ndescription: a gated harness skill\n---\nDo the task carefully.",
    )
    .expect("skill file");
    Arc::new(SkillSet::load_dir("docs", dir.path()).await)
}

fn docs_worker() -> runic::subagent::SubagentDraft {
    runic::subagent::subagent(DOCS_WORKER, "a gated harness worker")
        .prompt("you are a harness worker")
        .max_turns(3)
}

struct ChildProvider;

#[async_trait]
impl Provider for ChildProvider {
    fn name(&self) -> &str {
        "child"
    }
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(text_response("child done".to_string()))
    }
}

struct ChildBuilder;

#[async_trait]
impl SubagentBuilder for ChildBuilder {
    async fn provider(&self, _req: &SubagentReq<'_>) -> Arc<dyn Provider> {
        Arc::new(ChildProvider)
    }
    fn default_model(&self, _req: &SubagentReq<'_>) -> String {
        "child-model".to_string()
    }
    async fn tool_pool(&self, _req: &SubagentReq<'_>) -> Vec<Arc<dyn Tool>> {
        vec![]
    }
}

pub struct MarkerHook;

#[async_trait]
impl WriteHook for MarkerHook {
    fn name(&self) -> &str {
        "marker"
    }
    async fn before_agent(&self, state: &mut AgentState) -> HookOutcome {
        let _ = state.update("hook_fired", serde_json::json!("before_agent"));
        HookOutcome::Continue
    }
}

pub struct ScriptedProvider;

#[async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        // `req.messages` is the WHOLE thread's history, not just this run's —
        // a new run on a reused thread starts with every prior run's turns
        // still in front of it. So the directive is the LAST Text-bearing
        // user message, and "what happened this run" is only what follows it;
        // scanning the full history would see a previous run's tool results
        // (e.g. a prior `ask_user` answer) and short-circuit before this run
        // ever calls its own tools.
        let Some(directive_idx) = req
            .messages
            .iter()
            .rposition(|m| matches!(m.role, Role::User) && directive_text(m).is_some())
        else {
            return Ok(text_response("ok".to_string()));
        };
        let directive = directive_text(&req.messages[directive_idx]).unwrap_or_default();
        let this_run = &req.messages[directive_idx + 1..];

        let completed = completed_tool_names(this_run);
        Ok(respond(plan(directive.trim()), &completed, this_run))
    }
}

fn directive_text(message: &Message) -> Option<String> {
    match &message.content {
        MessageContent::Text(text) => Some(text.clone()),
        MessageContent::Blocks(blocks) => blocks.iter().find_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        }),
    }
}

fn completed_tool_names(messages: &[Message]) -> HashSet<String> {
    messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_name, .. } => Some(tool_name.clone()),
            _ => None,
        })
        .collect()
}

fn result_content_for(messages: &[Message], tool_name: &str) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            _ => None,
        })
        .flatten()
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_name: name,
                content,
                ..
            } if name == tool_name => Some(content.text()),
            _ => None,
        })
}

enum Step {
    Idle(String),
    Direct(&'static str, serde_json::Value),
    Deferred {
        ability: &'static str,
        tool: &'static str,
        args: serde_json::Value,
    },
}

fn plan(directive: &str) -> Step {
    if directive == "fail" {
        return Step::Direct("fail", serde_json::json!({}));
    }
    if directive == "delegate" {
        return Step::Deferred {
            ability: DOCS_ABILITY,
            tool: "delegate",
            args: serde_json::json!({ "agent": DOCS_WORKER, "prompt": "go" }),
        };
    }
    match directive.split_once(':') {
        Some(("add", rest)) => {
            let (a, b) = rest.split_once(',').unwrap_or(("0", "0"));
            Step::Direct(
                "add",
                serde_json::json!({
                    "a": a.trim().parse::<i64>().unwrap_or(0),
                    "b": b.trim().parse::<i64>().unwrap_or(0),
                }),
            )
        }
        Some(("slow", ms)) => Step::Direct(
            "slow",
            serde_json::json!({ "ms": ms.trim().parse::<u64>().unwrap_or(0) }),
        ),
        Some(("echo", text)) => Step::Deferred {
            ability: ECHO_ABILITY,
            tool: "echo",
            args: serde_json::json!({ "text": text }),
        },
        Some(("skill", _)) => Step::Deferred {
            ability: DOCS_ABILITY,
            tool: "skill_view",
            args: serde_json::json!({ "name": DOCS_SKILL_ID }),
        },
        Some(("ask", question)) => {
            Step::Direct("ask_user", serde_json::json!({ "question": question }))
        }
        Some(("say", text)) => Step::Idle(text.to_string()),
        _ => Step::Idle(if directive.is_empty() {
            "ok".to_string()
        } else {
            directive.to_string()
        }),
    }
}

fn respond(step: Step, completed: &HashSet<String>, this_run: &[Message]) -> CompletionResponse {
    match step {
        Step::Idle(text) => text_response(text),
        Step::Direct(name, args) => {
            if completed.contains(name) {
                let result = result_content_for(this_run, name).unwrap_or_default();
                text_response(format!("done: {result}"))
            } else {
                tool_call(name, args)
            }
        }
        Step::Deferred {
            ability,
            tool,
            args,
        } => {
            if completed.contains(tool) {
                let result = result_content_for(this_run, tool).unwrap_or_default();
                text_response(format!("done: {result}"))
            } else if completed.contains("load_ability") {
                tool_call(tool, args)
            } else {
                tool_call("load_ability", serde_json::json!({ "id": ability }))
            }
        }
    }
}

fn tool_call(name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: "call-1".into(),
            name: name.into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "call-1".into(),
            name: name.into(),
            input,
        }],
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..TokenUsage::default()
        },
    }
}

fn text_response(text: String) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text,
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..TokenUsage::default()
        },
    }
}

pub struct AddTool;

#[async_trait]
impl Tool for AddTool {
    fn name(&self) -> &str {
        "add"
    }
    fn description(&self) -> &str {
        "add two integers a and b"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
            "required": ["a", "b"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let a = args.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
        let b = args.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
        Ok(ToolResult::ok((a + b).to_string()))
    }
}

pub struct SlowTool;

#[async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "sleep for ms milliseconds, then return"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "ms": { "type": "integer" } },
            "required": ["ms"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let ms = args
            .get("ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(60_000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(ToolResult::ok(format!("slept {ms}ms")))
    }
}

pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo the given text back"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok(
            args.get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ))
    }
}

pub struct FailTool;

#[async_trait]
impl Tool for FailTool {
    fn name(&self) -> &str {
        "fail"
    }
    fn description(&self) -> &str {
        "always returns an error result"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::error("intentional failure"))
    }
}
