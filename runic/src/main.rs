//! runic — reference binary. Two surfaces over one shared harness:
//!
//!   runic            # interactive REPL (default)
//!   runic serve      # HTTP server (runic-serve)
//!
//! All agent wiring lives in [`harness::Harness`]; both surfaces build
//! from it so they expose the same fully-equipped agent.

mod config;
mod demo_tools;
mod harness;
mod hooks;
mod repl;
mod serve;

use anyhow::Result;

use crate::config::RunicConfig;
use crate::harness::Harness;

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(path) = dotenvy::dotenv().ok() {
        eprintln!("[env] loaded from {}", path.display());
    }

    let mode = std::env::args().nth(1).unwrap_or_default();

    let config = RunicConfig::from_env();
    let harness = Harness::load(config).await?;

    match mode.as_str() {
        "serve" => serve::run(harness).await,
        "" | "repl" => repl::run(harness).await,
        other => {
            anyhow::bail!("unknown subcommand '{other}' (expected: repl | serve)");
        }
    }
}
