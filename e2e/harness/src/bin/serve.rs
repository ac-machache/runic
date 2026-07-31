use runic::builtin::{CalculatorTool, SystemTimeTool};
use runic::composer::Agent;
use runic_serve::{PgPool, ServeConfig};

const ARTIFACT_ROOT: &str = "./artifacts";
const ADDR: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,runic=info,runic_serve=info".into()),
        )
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| anyhow::anyhow!("DATABASE_URL is not set (harness/.env)"))?;

    let sessions = runic::substrate::sessions_postgres(&database_url).await?;
    let blobs = runic::substrate::blobs_local(ARTIFACT_ROOT);
    let pool = PgPool::connect(&database_url).await?;

    let agent = Agent::new(runic::llm("mistral:mistral-small-latest")?)
        .tool(CalculatorTool)
        .tool(SystemTimeTool);

    println!("runic-serve on http://{ADDR}");
    println!("  curl -XPOST http://{ADDR}/threads -H 'content-type: application/json' -d '{{\"thread_id\":\"web\"}}'");
    println!("  curl -XPOST http://{ADDR}/threads/web/runs/wait -H 'content-type: application/json' -d '{{\"message\":\"hello\"}}'");

    runic_serve::serve(
        ServeConfig::new(sessions, blobs, pool).agent("main", agent),
        ADDR,
    )
    .await
}
