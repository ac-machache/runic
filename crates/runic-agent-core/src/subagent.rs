//! Synchronous subagent — a plain `Tool` that runs a child `Agent` to
//! completion and returns its final assistant text as the tool result.
//!
//! Inspired by `learn-claude-code-rs/s04_subagent`, but generalized: instead
//! of a single hardcoded "task" subagent, this is a generic combinator. You
//! create one `SubagentTool` per kind of subagent (research, planner, code
//! reviewer, etc.) with a factory closure that knows how to build the right
//! child `Agent`. Each instance is a distinct tool the model can choose.
//!
//! The parent sees the subagent as just another tool call; the child has its
//! own fresh `AgentState`, its own (typically smaller) tool set, and its own
//! system prompt focused on the delegated task. Exploratory context stays in
//! the child and never pollutes the parent transcript — only the summary
//! returns.
//!
//! Because `SubagentTool` is a `Tool` (not `HitlTool`), it is
//! `parallelizable() == true` by default. If the model emits multiple
//! subagent calls in one turn, they fan out concurrently via the parallel
//! dispatch path. Cheap fan-out exploration falls out for free.
//!
//! ## Recursion safety
//!
//! No built-in recursion limiter. If you want to forbid a subagent from
//! spawning subagents of its own, simply don't include any `SubagentTool` in
//! the factory's `AgentBuilder`. That's the s04 default and a sensible v0.

use async_trait::async_trait;
use std::sync::Arc;

use crate::agent::Agent;
use runic_tool_core::{BackgroundTool, Tool, ToolContext, ToolResult};

/// A subagent tool. Construct one per "kind" of subagent with a factory
/// closure that knows how to build a fresh child `Agent` per invocation.
pub struct SubagentTool {
    name: String,
    description: String,
    factory: Arc<dyn Fn() -> Agent + Send + Sync>,
}

impl SubagentTool {
    /// `factory` is called once per invocation. It must return a fully-built
    /// `Agent` ready to receive a prompt.
    pub fn new<F>(name: impl Into<String>, description: impl Into<String>, factory: F) -> Self
    where
        F: Fn() -> Agent + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            factory: Arc::new(factory),
        }
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Instructions for the subagent. Be specific and self-contained — the subagent has a fresh context and cannot see your conversation."
                }
            },
            "required": ["prompt"],
            "additionalProperties": false,
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        run_subagent(&self.name, input, self.factory.as_ref()).await
    }
}

/// Async / non-blocking subagent. Same shape as `SubagentTool` but implements
/// `BackgroundTool`, so the parent's loop returns a `task_id` immediately and
/// the child runs to completion in a tokio task. The parent polls via the
/// auto-registered `background_status` tool.
///
/// Use this when subagent work might take a while and you want the parent to
/// keep doing other things meanwhile. `SubagentTool` (synchronous) is still
/// the right choice for short, focused delegations where you need the answer
/// before continuing.
pub struct AsyncSubagentTool {
    name: String,
    description: String,
    factory: Arc<dyn Fn() -> Agent + Send + Sync>,
}

impl AsyncSubagentTool {
    pub fn new<F>(name: impl Into<String>, description: impl Into<String>, factory: F) -> Self
    where
        F: Fn() -> Agent + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            factory: Arc::new(factory),
        }
    }
}

#[async_trait]
impl BackgroundTool for AsyncSubagentTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Instructions for the async subagent. It runs in the background and you'll get a task_id back; use background_status to check on it."
                }
            },
            "required": ["prompt"],
            "additionalProperties": false,
        })
    }

    async fn run(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        run_subagent(&self.name, input, self.factory.as_ref()).await
    }
}

/// Shared body: pull the prompt, build a fresh child via the factory, run it,
/// return the final assistant text (or a useful error / empty-response note).
async fn run_subagent(
    name: &str,
    input: serde_json::Value,
    factory: &(dyn Fn() -> Agent + Send + Sync),
) -> ToolResult {
    let Some(prompt) = input.get("prompt").and_then(|v| v.as_str()) else {
        return ToolResult::error("missing required field 'prompt'");
    };

    let mut child = factory();
    match child.run(prompt).await {
        Ok(_outcome) => match child.state().last_assistant_text() {
            Some(text) => ToolResult::ok(text),
            None => ToolResult::ok(format!("(subagent '{name}' produced no text response)")),
        },
        Err(err) => ToolResult::error(format!("subagent '{name}' failed: {err}")),
    }
}
