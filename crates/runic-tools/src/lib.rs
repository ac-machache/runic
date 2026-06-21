//! `runic-tools` — the agent's built-in, standalone tools.
//!
//! Native tools that don't belong to a specific subsystem:
//! - **filesystem** — `read_file` / `write_file` / `edit_file` / `ls` / `glob`
//!   / `grep` + `apply_patch`, thin wrappers over a
//!   [`FilesystemBackend`](runic_filesystem::FilesystemBackend);
//! - **utility** — `calculator`, `system_time`.
//! - **web** — `web_fetch` (SSRF-guarded) + `web_search` (over a pluggable
//!   [`web::SearchProvider`]).
//! - **weather** — `weather` (current + 7-day forecast) and `weather_history`
//!   (past daily conditions, back to 1940), keyless via Open-Meteo.
//! - **human-in-the-loop** — `ask_user` + `escalate_to_human`, over the
//!   per-run [`runic_tool::HumanInterface`] the surface wires in.
//! - **integrations** — `composio`, one meta-tool over Composio's 1000+
//!   external app actions (needs an API key, so the app constructs it).
//!
//! Web and HITL tools are *not* in [`default_tools`]: `web_search` needs a
//! provider and the HITL tools only do anything once a human channel is wired,
//! so the app constructs and registers them explicitly.
//!
//! Subsystem-bound tools live with their subsystem (`skill_view`,
//! `delegate`, `tool_search`/MCP, `search_chats`). The app composes those with
//! [`default_tools`] into one registry.

use std::sync::Arc;

use runic_filesystem::FilesystemBackend;
use runic_tool::Tool;

pub mod calc;
pub mod composio;
pub mod fs;
pub mod hitl;
pub mod patch;
pub mod time;
pub mod weather;
pub mod web;

pub use calc::CalculatorTool;
pub use composio::ComposioTool;
pub use fs::{
    fs_tools, EditFileTool, GlobTool, GrepTool, LsTool, ReadFileTool, WriteFileTool,
};
pub use hitl::{AskUserTool, EscalateToHumanTool};
pub use patch::ApplyPatchTool;
pub use time::SystemTimeTool;
pub use weather::{WeatherHistoryTool, WeatherTool};

// Pure parsers exposed for fuzz/property testing — not part of the stable API.
#[doc(hidden)]
pub use calc::eval as eval_calc;
#[doc(hidden)]
pub use web::{decode_entities, html_to_text};
pub use web::{SearchProvider, SearchResult, SearxngProvider, TavilyProvider, WebFetchTool, WebSearchTool};

/// The native tools bound to a filesystem backend: the six fs tools +
/// `apply_patch` + `calculator` + `system_time`. The app adds subsystem tools
/// (skills/MCP/subagents/search) and registers the lot on the agent.
pub fn default_tools(fs: Arc<dyn FilesystemBackend>) -> Vec<Arc<dyn Tool>> {
    let mut tools = fs_tools(fs.clone());
    tools.push(Arc::new(ApplyPatchTool(fs)));
    tools.push(Arc::new(CalculatorTool));
    tools.push(Arc::new(SystemTimeTool));
    tools
}
