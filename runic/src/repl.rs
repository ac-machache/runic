//! Interactive REPL surface: reads prompts from stdin, streams agent
//! events to the terminal, and carries one agent's state across inputs.

use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
use runic_agent_core::AgentEvent;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_stream::StreamExt;

use crate::demo_tools::StdinApprover;
use crate::harness::Harness;

/// Run the REPL against a freshly-built agent from `harness`.
pub async fn run(harness: Harness) -> Result<()> {
    let approver = Arc::new(StdinApprover);
    let mut agent = harness.build_agent(None, None, Some(approver));
    let command_registry = harness.command_registry().clone();

    if harness.config.persist {
        eprintln!(
            "[persist] events → sessions/{}/{}/events.jsonl",
            harness.config.tenant,
            agent.state().session_id
        );
    }

    eprintln!(
        "runic — model={}, tools={:?}, session={}",
        harness.provider_model(),
        agent.tools().names(),
        agent.state().session_id
    );
    eprintln!(
        "commands: /state (summary)  /dump (full JSON)  /quit | /exit | Ctrl-D{}\n",
        if command_registry.is_empty() {
            String::new()
        } else {
            format!(
                "  + {}",
                command_registry.list().iter().map(|c| format!("/{}", c.meta.name)).collect::<Vec<_>>().join(" ")
            )
        }
    );

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();

    loop {
        prompt();
        let line = tokio::select! {
            line = reader.next_line() => line?,
            _ = tokio::signal::ctrl_c() => { eprintln!("\n(interrupted)"); return Ok(()); }
        };
        let Some(line) = line else {
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
                s.session_id, s.events.len(), s.runs().len(), s.messages_for_provider().len()
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

        // User-defined slash command? Expand its template. Unknown → list.
        let input: String = match runic_commands::split_invocation(trimmed) {
            Some((name, args)) => match command_registry.get(name) {
                Some(cmd) => cmd.expand(args),
                None => {
                    let available: Vec<&str> =
                        command_registry.list().iter().map(|c| c.meta.name.as_str()).collect();
                    eprintln!(
                        "[commands] unknown command /{name} — available: {available:?} (plus builtins /state /dump /quit)"
                    );
                    continue;
                }
            },
            None => trimmed.to_string(),
        };

        let (mut events, handle) = agent.run_streaming(&input);
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
                    Ok(outcome) => eprintln!(
                        "\n[done turns={} stop={:?} input_tokens={:?} output_tokens={:?}]",
                        outcome.total_turns, outcome.stop_reason,
                        outcome.usage.input_tokens, outcome.usage.output_tokens
                    ),
                    Err(err) => eprintln!("\n[error] {err}"),
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
            eprintln!("[tool dispatch] {}({})", call.name, compact_json(&call.input));
        }
        AgentEvent::ToolFinished { call, result, duration_ms } => {
            let tag = if result.is_error { "ERROR" } else { "ok" };
            eprintln!("[tool finish] {} {} in {}ms → {}", call.name, tag, duration_ms, truncate(&result.content, 200));
        }
        AgentEvent::Usage(_) => {}
        AgentEvent::TurnComplete { stop_reason, tool_calls_this_turn } => {
            if state.in_thinking {
                eprintln!();
                state.in_thinking = false;
            }
            println!();
            eprintln!("[turn complete] stop={:?} tool_calls={}", stop_reason, tool_calls_this_turn);
        }
        AgentEvent::RunComplete { total_turns: _ } => {}
        AgentEvent::Warning(msg) => eprintln!("[warning] {msg}"),
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
