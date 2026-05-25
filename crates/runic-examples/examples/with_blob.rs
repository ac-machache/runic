//! Upload a file as a content-addressed blob and ask the agent about it.
//!
//! Shows the full programmatic path: open the file → put into the
//! BlobStore → embed the resulting BlobRef in a message → ship to the
//! agent, which automatically materializes blob refs to inline image
//! data via `BlobMaterializingProvider`.
//!
//! Pass an image path as the first arg (defaults to a 1x1 PNG generated
//! at runtime so the example works without external files):
//! ```sh
//! ANTHROPIC_API_KEY=sk-... cargo run --example with_blob -- /path/to/photo.png
//! ```

use std::sync::Arc;

use anyhow::{Context, Result};
use runic_agent_core::Agent;
use runic_blobs::{
    BlobInput, BlobMaterializingProvider, BlobStore, BlobStoreResolver, FileBlobStore,
};
use runic_message_types::{ContentBlock, Message, Role};
use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use runic_provider_core::Provider;
use runic_storage_backend::{LocalFsBackend, StorageBackend};
use tempfile::TempDir;

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable must be set")?;

    // Ephemeral storage under a tempdir so the example doesn't pollute
    // ~/.runic. In a real app you'd pass a persistent LocalFsBackend.
    let tmp = TempDir::new()?;
    let storage: Arc<dyn StorageBackend> = Arc::new(LocalFsBackend::new(tmp.path()));
    let blob_store: Arc<dyn BlobStore> = Arc::new(FileBlobStore::new(storage));

    // Wrap the provider so blob refs in messages get materialized
    // automatically — see crates/runic-blobs/src/provider.rs.
    let raw_provider: Arc<dyn Provider> =
        AnthropicProvider::new(AnthropicConfig::new(api_key));
    let provider: Arc<dyn Provider> = Arc::new(BlobMaterializingProvider::new(
        raw_provider,
        Arc::new(BlobStoreResolver::new(blob_store.clone(), "demo")),
    ));

    // ─── Load image bytes ───────────────────────────────────────────────
    let path = std::env::args().nth(1);
    let (bytes, mime, name) = match path {
        Some(p) => {
            let bytes = tokio::fs::read(&p)
                .await
                .with_context(|| format!("reading {p}"))?;
            let mime = guess_mime(&p);
            (bytes, mime, Some(p))
        }
        None => {
            // 1x1 red PNG — works without any external files.
            let bytes = vec![
                0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49,
                0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02,
                0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44,
                0x41, 0x54, 0x08, 0x99, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03, 0x00,
                0x01, 0x5B, 0xCC, 0x82, 0x83, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
                0xAE, 0x42, 0x60, 0x82,
            ];
            (bytes, "image/png".to_string(), Some("tiny.png".to_string()))
        }
    };

    // ─── Upload the file as a blob ──────────────────────────────────────
    let mut input = BlobInput::new(bytes.clone(), &mime);
    if let Some(n) = name {
        input = input.with_name(n);
    }
    let blob_ref = blob_store.put("demo", input).await?;
    println!(
        "[blob] uploaded {} bytes, mime={}, id={}",
        blob_ref.size, blob_ref.mime, blob_ref.id
    );

    // ─── Build a message that references the blob ───────────────────────
    let user_msg = Message {
        role: Role::User,
        content: vec![
            ContentBlock::Blob(blob_ref.clone()),
            ContentBlock::Text {
                text: "Describe this image in one sentence.".into(),
                cache_control: None,
            },
        ],
        timestamp: Some(chrono::Utc::now()),
        tool_duration_ms: None,
    };

    // ─── Drive the agent with that message ──────────────────────────────
    let mut agent = Agent::builder(provider)
        .system_prompt("You are a concise visual assistant.")
        .build();

    // Push the user message directly into state, then drive a turn with
    // empty input. (For a "normal" text-only run you'd call
    // agent.run(text) directly — but text-only doesn't support
    // multi-block messages.)
    agent
        .state_mut()
        .push_event(runic_agent_core::SessionEvent::Message {
            run_id: uuid::Uuid::new_v4().to_string(),
            msg: user_msg,
            at: chrono::Utc::now(),
        });

    // Now run; since there's already a user message in state, the
    // agent will respond to it.
    let outcome = agent.run("").await?;
    println!(
        "[done: {} turn(s), stop={:?}]",
        outcome.total_turns, outcome.stop_reason
    );
    if let Some(text) = agent.state().last_assistant_text() {
        println!("\nassistant: {text}");
    }

    Ok(())
}

fn guess_mime(path: &str) -> String {
    let lower = path.to_lowercase();
    if lower.ends_with(".png") {
        "image/png".into()
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".into()
    } else if lower.ends_with(".gif") {
        "image/gif".into()
    } else if lower.ends_with(".webp") {
        "image/webp".into()
    } else if lower.ends_with(".pdf") {
        "application/pdf".into()
    } else {
        "application/octet-stream".into()
    }
}
