//! Proves the whole SDK is reachable through one `runic-foundry` dependency —
//! no consumer should need to add `runic-agent`, `runic-tool`, etc. directly
//! just to write code against them.

use runic::agent::{Agent, AgentBuilder};
use runic::hook::{ReadHook, WriteHook};
use runic::mcp::McpClient;
use runic::memory::MemoryStore;
use runic::provider::Provider;
use runic::skills::SkillSet;
use runic::state::AgentState;
use runic::subagent::AgentRoster;
use runic::substrate::SessionStore;
use runic::tool::{Tool, ToolContext, ToolResult};
use runic::tools::default_tools;
use runic::types::Message;

fn assert_types_reachable() {
    fn _agent(_: Agent) {}
    fn _agent_builder(_: AgentBuilder) {}
    fn _read_hook(_: &dyn ReadHook) {}
    fn _write_hook(_: &dyn WriteHook) {}
    fn _mcp_client(_: McpClient) {}
    fn _memory_store(_: MemoryStore) {}
    fn _provider(_: &dyn Provider) {}
    fn _skill_set(_: SkillSet) {}
    fn _agent_state(_: AgentState) {}
    fn _agent_roster(_: AgentRoster) {}
    fn _session_store(_: &dyn SessionStore) {}
    fn _tool(_: &dyn Tool) {}
    fn _tool_context(_: ToolContext) {}
    fn _tool_result(_: ToolResult) {}
    fn _message(_: Message) {}
    let _ = default_tools;
}

#[test]
fn umbrella_surface_compiles() {
    assert_types_reachable();
}

#[cfg(feature = "anthropic")]
fn _anthropic_reachable(_: runic::provider::AnthropicDriver) {}
#[cfg(feature = "openai")]
fn _openai_reachable(_: runic::provider::openai::OpenAIDriver) {}
#[cfg(feature = "mistral")]
fn _mistral_reachable(_: runic::provider::MistralDriver) {}
#[cfg(feature = "gemini")]
fn _gemini_reachable(_: runic::provider::gemini::GeminiDriver) {}
