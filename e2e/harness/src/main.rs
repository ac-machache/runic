use std::io::Write;

use std::sync::Arc;

use runic::Input;
use runic::builtin::{CalculatorTool, SystemTimeTool};
use runic::composer::Agent;
use runic::state::{AgentEvent, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Debug)]
struct Printer;

impl Emitter for Printer {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(text) => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            other => println!("\n  {other:?}"),
        }
    }
}

const ARTIFACT_ROOT: &str = "./artifacts";
const TENANT: &str = "local";
const THREAD: &str = "repl";

fn media_type(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn attachment(rest: &str) -> anyhow::Result<Input> {
    let (path, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let bytes = std::fs::read(path)?;
    let mime = media_type(path);
    println!("  attaching {path} — {} bytes, {mime}", bytes.len());
    let text = if text.trim().is_empty() {
        "what is this"
    } else {
        text.trim()
    };
    let input = Input::text(text);
    Ok(if mime.starts_with("image/") {
        input.image(mime, &bytes)
    } else {
        input.file(mime, &bytes)
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,runic=info".into()),
        )
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| anyhow::anyhow!("DATABASE_URL is not set (harness/.env)"))?;
    let host = database_url
        .rsplit('@')
        .next()
        .unwrap_or(&database_url)
        .to_string();
    println!("connecting to postgres at {host} …");
    std::io::stdout().flush()?;
    let sessions = runic::substrate::sessions_postgres(&database_url).await?;
    let store = sessions.store();
    let blobs = runic::substrate::blobs_local(ARTIFACT_ROOT);

    let agent = Agent::new(runic::llm("mistral:mistral-small-latest")?)
        .tool(CalculatorTool)
        .tool(SystemTimeTool);
    let chat = runic::session((TENANT, THREAD))
        .store(sessions)
        .artifacts(blobs);

    let mut seen = store.read(TENANT, THREAD).await?.len();
    println!("runic repl — postgres session store, local artifacts at {ARTIFACT_ROOT}");
    println!("thread {TENANT}/{THREAD}, {seen} events already on it.");
    println!("`/file <path> [message]` attaches a file. ctrl-d to exit.\n");

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

        let input = match line.strip_prefix("/file ") {
            Some(rest) => match attachment(rest.trim()) {
                Ok(input) => input,
                Err(error) => {
                    println!("\ncannot attach: {error}\n");
                    continue;
                }
            },
            None => Input::text(line),
        };

        match chat.invoke(&agent, input.events(Arc::new(Printer))).await {
            Ok(answer) => println!("\n\n{}\n", answer.text),
            Err(error) => println!("\nerror: {error}\n"),
        }

        seen = store.read(TENANT, THREAD).await?.len();
        println!("  ({seen} events persisted on this thread)\n");
    }

    Ok(())
}
