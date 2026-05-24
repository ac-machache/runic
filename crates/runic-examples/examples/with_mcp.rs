//! Connect to MCP servers configured in `~/.runic/mcp.json` and let the
//! agent use their tools alongside its native ones.
//!
//! Requires an MCP server to be available. The simplest one to try:
//! ```sh
//! mkdir -p ~/.runic
//! cat > ~/.runic/mcp.json <<'EOF'
//! {
//!   "mcpServers": {
//!     "filesystem": {
//!       "command": "uvx",
//!       "args": ["mcp-server-filesystem", "/tmp"]
//!     }
//!   }
//! }
//! EOF
//! ANTHROPIC_API_KEY=sk-... cargo run --example with_mcp
//! ```

use anyhow::{Context, Result};
use runic_agent_core::Agent;
use runic_mcp::{McpConfig, McpManager};
use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use runic_provider_core::Provider;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable must be set")?;
    let provider: Arc<dyn Provider> = AnthropicProvider::new(AnthropicConfig::new(api_key));

    // Discover mcp.json.
    let home = std::env::var("RUNIC_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let mut p = std::env::var("HOME").map(std::path::PathBuf::from).unwrap_or_default();
            p.push(".runic");
            p
        });
    let mcp_path = home.join("mcp.json");

    let config = McpConfig::try_load_from_path(&mcp_path).await?.unwrap_or_default();
    if config.is_empty() {
        eprintln!(
            "no mcpServers configured in {} — example will run with no MCP tools",
            mcp_path.display()
        );
    }

    // Connect every configured server concurrently. Per-server failures
    // are logged and skipped — manager only contains servers that came up.
    let manager = McpManager::connect_all(&config).await;
    eprintln!(
        "[mcp] {} server(s) connected, {} tool(s) total: {:?}",
        manager.len(),
        manager.total_tool_count(),
        manager.server_names()
    );

    // Register each MCP tool with the agent. They show up under names
    // like `mcp__filesystem__read_file`.
    let mut builder = Agent::builder(provider).system_prompt(
        "You have access to MCP tools (prefixed `mcp__<server>__<tool>`). \
         Use them when relevant. Reply concisely.",
    );
    for tool in manager.all_tools() {
        builder = builder.tool(tool);
    }
    let mut agent = builder.build();

    let outcome = agent
        .run("List the files in /tmp using a filesystem tool, if you have one.")
        .await?;
    println!(
        "\n[done: {} turn(s), stop={:?}]",
        outcome.total_turns, outcome.stop_reason
    );
    if let Some(text) = agent.state().last_assistant_text() {
        println!("\nassistant: {text}");
    }

    // Clean shutdown of MCP subprocesses.
    manager.shutdown_all().await;
    Ok(())
}
