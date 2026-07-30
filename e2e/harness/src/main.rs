use std::io::Write;
use std::sync::Arc;

use runic::builtin::{CalculatorTool, SystemTimeTool};
use runic::composer::Agent;
use runic::state::{AgentEvent, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug)]
struct Printer;

impl Emitter for Printer {
    fn emit(&self, event: AgentEvent) {
        println!("  {event:?}");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let agent = Agent::new(runic::llm("mistral:mistral-small-latest")?)
        .tool(CalculatorTool)
        .tool(SystemTimeTool);

    println!("runic repl — every line is a fresh agent, no history. ctrl-d to exit.\n");

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        print!("› ");
        std::io::stdout().flush()?;

        let Some(line) = lines.next_line().await? else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "exit" | "quit") {
            break;
        }

        match agent.stream(line, Arc::new(Printer)).await {
            Ok(answer) => println!("\n{}\n", answer.text),
            Err(error) => println!("\nerror: {error}\n"),
        }
    }

    Ok(())
}
