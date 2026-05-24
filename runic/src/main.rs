//! runic — minimal main binary that drives the agent loop interactively.
//!
//! Reads a prompt from stdin, runs the agent to completion (potentially many
//! tool-calling turns), prints streamed events as they arrive, and loops so
//! the same agent state carries across user inputs.
//!
//! Quit with `/quit`, `/exit`, EOF (Ctrl-D), or Ctrl-C.

use std::io::Write;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use runic_agent_core::{
    Agent, AgentConfig, AgentEvent, AgentState, ApprovalRequest, Approver, ApproverHandle,
    AsyncSubagentTool, BackgroundTool, Draft, HitlTool, Hook, HookOutcome, SubagentTool, Tool,
    ToolContext, ToolResult, UserDecision,
};
use runic_context_engine::{
    BasePromptLayer, CompositeEngine, MemoryLayer, PersonaLayer, UserFactsLayer,
};
use runic_message_types::ToolCall;
use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use runic_provider_core::Provider;
use runic_provider_gemini::{GeminiConfig, GeminiProvider};
use runic_skills::{SkillRegistry, SkillViewTool, SkillsIndexLayer};
use runic_storage_backend::{LocalFsBackend, StorageBackend};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_stream::StreamExt;
use uuid::Uuid;

/// Typed wrapper so the runtime bag can be keyed by exactly this type
/// (rather than colliding with any other `Uuid` someone might stash).
#[derive(Debug, Clone)]
struct SessionUuid(Uuid);

const DEFAULT_SYSTEM_PROMPT: &str = "\
You are a focused test assistant for the runic agent harness.

Keep replies short. When asked to demonstrate a tool, use it; otherwise reply directly.
Available tools:
  - echo: returns the message you pass in (useful to confirm tool dispatch works).
";

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env from cwd (and walk up). Silently ignored if absent.
    let env_path = dotenvy::dotenv().ok();

    if let Some(path) = env_path {
        eprintln!("[env] loaded from {}", path.display());
    }

    // Provider selection: RUNIC_PROVIDER=anthropic (default) or gemini.
    // Each provider has its own *_API_KEY env var. RUNIC_MODEL optionally
    // overrides whichever provider's default model.
    let kind = std::env::var("RUNIC_PROVIDER")
        .unwrap_or_else(|_| "anthropic".into())
        .to_lowercase();
    let model_override = std::env::var("RUNIC_MODEL").ok();

    let provider: Arc<dyn Provider> = match kind.as_str() {
        "anthropic" => {
            let key = std::env::var("ANTHROPIC_API_KEY").context(
                "ANTHROPIC_API_KEY must be set when RUNIC_PROVIDER=anthropic",
            )?;
            let mut cfg = AnthropicConfig::new(key);
            if let Some(m) = model_override {
                cfg = cfg.with_model(m);
            }
            AnthropicProvider::new(cfg)
        }
        "gemini" => {
            let key = std::env::var("GEMINI_API_KEY")
                .context("GEMINI_API_KEY must be set when RUNIC_PROVIDER=gemini")?;
            let mut cfg = GeminiConfig::new(key);
            if let Some(m) = model_override {
                cfg = cfg.with_model(m);
            }
            GeminiProvider::new(cfg)
        }
        other => {
            anyhow::bail!("unknown RUNIC_PROVIDER='{other}' (expected: anthropic | gemini)");
        }
    };

    let session_uuid = SessionUuid(Uuid::new_v4());
    let session_uuid_for_print = session_uuid.0;
    let approver: ApproverHandle = Arc::new(StdinApprover);

    // ── Context engine: file-backed SOUL / USER / MEMORY ────────────────
    // RUNIC_HOME overrides the default ~/.runic/ if set.
    let runic_home: std::path::PathBuf = std::env::var("RUNIC_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let mut p = dirs_home_or_cwd();
            p.push(".runic");
            p
        });
    eprintln!("[runic-home] {}", runic_home.display());

    let storage: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(runic_home.clone()));

    // ── Skills: scan ~/.runic/skills/ for {dir}/SKILL.md entries ───────
    // Each parsed skill becomes an entry in the trigger table the model
    // sees in the system prompt. The model loads bodies on demand via
    // the `skill_view` tool.
    let skill_registry = Arc::new(
        SkillRegistry::load(storage.clone(), "skills")
            .await
            .context("loading skills from ~/.runic/skills/")?,
    );
    eprintln!(
        "[skills] loaded {} skill(s): {:?}",
        skill_registry.len(),
        skill_registry.list().iter().map(|s| s.meta.name.as_str()).collect::<Vec<_>>()
    );

    let context_engine = CompositeEngine::new()
        .with_layer(BasePromptLayer::new(DEFAULT_SYSTEM_PROMPT))
        .with_layer(PersonaLayer::new(storage.clone(), "SOUL.md"))
        .with_layer(UserFactsLayer::new(storage.clone(), "memory/USER.md"))
        .with_layer(MemoryLayer::new(storage.clone(), "memory/MEMORY.md"))
        .with_layer(SkillsIndexLayer::new(skill_registry.clone()));

    // Helper closure-factory builder for any subagent kind we want.
    // Generic over the trait object so the same factory works for any provider.
    let make_subagent_factory =
        |provider: Arc<dyn Provider>, system_prompt: &'static str, max_turns: u32| {
            move || {
                Agent::builder(provider.clone())
                    .system_prompt(system_prompt)
                    .config(AgentConfig {
                        max_turns,
                        ..Default::default()
                    })
                    .build()
            }
        };

    // Synchronous: parent waits for the child's answer before continuing.
    // Good for quick focused delegations.
    let research_subagent = SubagentTool::new(
        "research_assistant",
        "Spawn a focused synchronous subagent that investigates the prompt and returns a concise summary. The parent waits for the answer. The subagent has fresh context — be self-contained in the prompt.",
        make_subagent_factory(
            provider.clone(),
            "You are a focused research subagent. Investigate the user's prompt \
             and return a concise summary in 3-6 lines. Do not ask clarifying \
             questions — make reasonable assumptions and answer.",
            8,
        ),
    );

    // Asynchronous: returns a task_id immediately; parent keeps going.
    // Poll progress with background_status, abort with background_cancel.
    let deep_research_subagent = AsyncSubagentTool::new(
        "deep_research",
        "Spawn an ASYNCHRONOUS subagent for longer investigations. Returns a task_id immediately so you can keep working; check progress with background_status(task_id) and read the result when status is 'done'.",
        make_subagent_factory(
            provider.clone(),
            "You are a deep research subagent. Take your time, explore the question \
             thoroughly, and produce a thorough multi-paragraph answer.",
            16,
        ),
    );

    let skill_view_tool = SkillViewTool::new(skill_registry.clone(), storage.clone(), "skills");

    let mut agent = Agent::builder(provider.clone())
        .system_prompt(DEFAULT_SYSTEM_PROMPT)
        .context_engine(context_engine)
        .tool(Arc::new(EchoTool))
        .tool(Arc::new(skill_view_tool))
        .tool(Arc::new(research_subagent))
        .hitl_tool(Arc::new(EmailTool))
        .background_tool(Arc::new(SlowCountTool))
        .background_tool(Arc::new(deep_research_subagent))
        .hook(Arc::new(LoggingHook))
        .runtime(session_uuid)
        .runtime(approver)
        .build();

    eprintln!(
        "runic — model={}, tools={:?}, runtime SessionUuid={}",
        provider.model(),
        agent.tools().names(),
        session_uuid_for_print
    );
    eprintln!("commands: /state (summary)  /dump (full JSON)  /quit | /exit | Ctrl-D\n");

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();

    loop {
        prompt();
        let line = tokio::select! {
            line = reader.next_line() => line?,
            _ = tokio::signal::ctrl_c() => { eprintln!("\n(interrupted)"); return Ok(()); }
        };
        let Some(line) = line else {
            // EOF
            eprintln!("\n(EOF)");
            return Ok(());
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if matches!(trimmed, "/quit" | "/exit") {
            return Ok(());
        }
        if trimmed == "/state" {
            let s = agent.state();
            eprintln!(
                "[state] session={} events={} runs={} messages={}",
                s.session_id,
                s.events.len(),
                s.runs().len(),
                s.messages_for_provider().len()
            );
            continue;
        }
        if trimmed == "/dump" {
            match serde_json::to_string_pretty(agent.state()) {
                Ok(json) => println!("{json}"),
                Err(err) => eprintln!("[dump error] {err}"),
            }
            continue;
        }

        let (mut events, handle) = agent.run_streaming(trimmed);
        let mut printer_state = PrinterState::default();

        tokio::pin! {
            let interrupt = tokio::signal::ctrl_c();
        }
        let mut interrupted = false;

        loop {
            tokio::select! {
                ev = events.next() => {
                    let Some(ev) = ev else { break };
                    print_event(&mut printer_state, ev);
                }
                _ = &mut interrupt, if !interrupted => {
                    eprintln!("\n(interrupt — waiting for in-flight turn to settle)");
                    interrupted = true;
                }
            }
        }

        match handle.await {
            Ok((returned_agent, outcome)) => {
                agent = returned_agent;
                match outcome {
                    Ok(outcome) => {
                        eprintln!(
                            "\n[done turns={} stop={:?} input_tokens={:?} output_tokens={:?}]",
                            outcome.total_turns,
                            outcome.stop_reason,
                            outcome.usage.input_tokens,
                            outcome.usage.output_tokens
                        );
                    }
                    Err(err) => {
                        eprintln!("\n[error] {err}");
                    }
                }
            }
            Err(join_err) => {
                eprintln!("\n[task error] {join_err}");
                return Err(join_err.into());
            }
        }

        if interrupted {
            return Ok(());
        }
    }
}

fn prompt() {
    eprint!("> ");
    std::io::stderr().flush().ok();
}

#[derive(Default)]
struct PrinterState {
    in_thinking: bool,
}

fn print_event(state: &mut PrinterState, event: AgentEvent) {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    match event {
        AgentEvent::AssistantTextDelta(text) => {
            if state.in_thinking {
                eprintln!();
                state.in_thinking = false;
            }
            print!("{text}");
            stdout.flush().ok();
        }
        AgentEvent::AssistantThinkingDelta(text) => {
            if !state.in_thinking {
                eprint!("\n[thinking] ");
                state.in_thinking = true;
            }
            eprint!("{text}");
            std::io::stderr().flush().ok();
        }
        AgentEvent::ToolUseStart { id, name } => {
            if state.in_thinking {
                eprintln!();
                state.in_thinking = false;
            }
            eprintln!("\n[tool start] {name} (id={id})");
        }
        AgentEvent::ToolDispatching(call) => {
            eprintln!(
                "[tool dispatch] {}({})",
                call.name,
                compact_json(&call.input)
            );
        }
        AgentEvent::ToolFinished {
            call,
            result,
            duration_ms,
        } => {
            let tag = if result.is_error { "ERROR" } else { "ok" };
            eprintln!(
                "[tool finish] {} {} in {}ms → {}",
                call.name,
                tag,
                duration_ms,
                truncate(&result.content, 200)
            );
        }
        AgentEvent::Usage(_) => {}
        AgentEvent::TurnComplete {
            stop_reason,
            tool_calls_this_turn,
        } => {
            if state.in_thinking {
                eprintln!();
                state.in_thinking = false;
            }
            println!();
            eprintln!(
                "[turn complete] stop={:?} tool_calls={}",
                stop_reason, tool_calls_this_turn
            );
        }
        AgentEvent::RunComplete { total_turns: _ } => {}
        AgentEvent::Warning(msg) => {
            eprintln!("[warning] {msg}");
        }
    }
}

fn compact_json(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "<unprintable>".into())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}… [+{} chars]", s.chars().count() - max)
    }
}

/// Demo hook — prints every lifecycle event to stderr and lets the loop
/// proceed normally. Useful for visually confirming hook ordering.
struct LoggingHook;

#[async_trait::async_trait]
impl Hook for LoggingHook {
    fn name(&self) -> &'static str {
        "logging"
    }

    async fn before_agent(&self, state: &mut AgentState) -> HookOutcome {
        eprintln!(
            "  [hook] before_agent  | events_so_far={}",
            state.events.len()
        );
        HookOutcome::Continue
    }

    async fn after_agent(&self, state: &mut AgentState) -> HookOutcome {
        eprintln!("  [hook] after_agent   | events_now={}", state.events.len());
        HookOutcome::Continue
    }

    async fn before_model(&self, _state: &mut AgentState) -> HookOutcome {
        eprintln!("  [hook] before_model");
        HookOutcome::Continue
    }

    async fn after_model(&self, _state: &mut AgentState, stop_reason: Option<&str>) -> HookOutcome {
        eprintln!("  [hook] after_model   | stop={:?}", stop_reason);
        HookOutcome::Continue
    }

    async fn before_tool(&self, _state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
        eprintln!(
            "  [hook] before_tool   | tool={} input={}",
            call.name,
            compact_json(&call.input)
        );
        HookOutcome::Continue
    }

    async fn after_tool(
        &self,
        _state: &mut AgentState,
        call: &ToolCall,
        result: &ToolResult,
    ) -> HookOutcome {
        eprintln!(
            "  [hook] after_tool    | tool={} is_error={} content_chars={}",
            call.name,
            result.is_error,
            result.content.chars().count()
        );
        HookOutcome::Continue
    }
}

/// Trivial echo tool. Lets us prove the full tool-dispatch round-trip works
/// without any external side effects.
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo the given message back to the assistant. Use this to verify tool dispatch."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The text to echo back"
                }
            },
            "required": ["message"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let Some(msg) = input.get("message").and_then(|v| v.as_str()) else {
            return ToolResult::error("missing required field 'message'");
        };
        // Pull the typed handle the binary registered via .runtime(SessionUuid(...)).
        // Returns None if no SessionUuid was registered — surface that explicitly so
        // we can see whether the runtime plumbing is working.
        match ctx.get::<SessionUuid>() {
            Some(uuid) => ToolResult::ok(format!("echo: {msg} | session_uuid={}", uuid.0)),
            None => ToolResult::error("SessionUuid not found in runtime context"),
        }
    }
}

struct EmailTool;

#[async_trait::async_trait]
impl HitlTool for EmailTool {
    fn name(&self) -> &str {
        "send_email"
    }
    fn description(&self) -> &str {
        "Send an email. The user is asked to fill in the recipient and may edit the subject/body before sending."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "recipient": { "type": "string", "description": "Email address (often left blank — the user will provide it)" },
                "subject":   { "type": "string" },
                "body":      { "type": "string" }
            },
            "required": ["subject", "body"]
        })
    }

    fn draft(&self, input: &serde_json::Value) -> Draft {
        let recipient = input
            .get("recipient")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let subject = input
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or("(no subject)");
        let summary = format!(
            "Send email\n  to:      {}\n  subject: {}",
            if recipient.is_empty() {
                "(blank — please fill)"
            } else {
                recipient
            },
            subject
        );
        Draft {
            summary,
            current_input: input.clone(),
            input_schema: self.input_schema(),
            editable_fields: vec!["recipient".into(), "subject".into(), "body".into()],
        }
    }

    async fn execute(&self, final_input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        // For the demo we don't actually send — just echo what would have been sent.
        let recipient = final_input
            .get("recipient")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let subject = final_input
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let body = final_input
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        ToolResult::ok(format!(
            "(simulated) email sent\n  to: {recipient}\n  subject: {subject}\n  body: {body}"
        ))
    }
}

struct StdinApprover;

#[async_trait::async_trait]
impl Approver for StdinApprover {
    async fn review(&self, req: ApprovalRequest) -> UserDecision {
        use std::io::Write as _;
        use tokio::io::{AsyncBufReadExt, BufReader};

        // Fresh reader — the REPL is awaiting us, so stdin is free.
        let mut reader = BufReader::new(tokio::io::stdin()).lines();

        eprintln!("\n--- HITL APPROVAL ---");
        eprintln!("Tool: {}", req.tool_name);
        eprintln!("{}", req.draft.summary);
        eprintln!("---");

        let mut current = req.draft.current_input.clone();

        for field in &req.draft.editable_fields {
            let cur_str = current.get(field).and_then(|v| v.as_str()).unwrap_or("");
            let display = if cur_str.is_empty() { "blank" } else { cur_str };
            eprint!("  {field} [{display}] (enter to keep): ");
            std::io::stderr().flush().ok();

            match reader.next_line().await {
                Ok(Some(line)) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        current[field] = serde_json::Value::String(trimmed.to_string());
                    }
                }
                _ => {
                    return UserDecision::Cancel {
                        reason: "stdin closed".into(),
                    };
                }
            }
        }

        eprint!("Send? [y/N]: ");
        std::io::stderr().flush().ok();
        match reader.next_line().await {
            Ok(Some(line)) if matches!(line.trim().to_lowercase().as_str(), "y" | "yes") => {
                UserDecision::Submit {
                    final_input: current,
                }
            }
            _ => UserDecision::Cancel {
                reason: "user declined".into(),
            },
        }
    }
}

// ─── SlowCountTool (BackgroundTool) ─────────────────────────────────────────

/// Trivial demo BackgroundTool. Sleeps `seconds` seconds then reports back.
/// Exercise: ask the model to count to 8, get the task id, do something else,
/// then ask for status via the auto-registered `background_status` tool.
struct SlowCountTool;

#[async_trait]
impl BackgroundTool for SlowCountTool {
    fn name(&self) -> &str {
        "slow_count"
    }
    fn description(&self) -> &str {
        "Start a background counter that finishes after `seconds` seconds. Returns a task_id immediately; use `background_status` with that id to check on it later."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 120,
                    "description": "How many seconds to count"
                }
            },
            "required": ["seconds"],
            "additionalProperties": false
        })
    }

    async fn run(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let seconds = input
            .get("seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .min(120);
        tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
        ToolResult::ok(format!("counted to {seconds}"))
    }
}

// ─── Home directory helper ──────────────────────────────────────────────────

/// Resolve the user's home directory (HOME on Unix, USERPROFILE on Windows).
/// Falls back to the current working directory if none can be found —
/// guarantees we never crash for missing env vars.
fn dirs_home_or_cwd() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return std::path::PathBuf::from(home);
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        return std::path::PathBuf::from(home);
    }
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}
