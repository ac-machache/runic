//! `ArtifactResolver` rewrites a request's `ArtifactRef` blocks: latest-user
//! refs inline with the STORED mime, older refs become reminders, and a
//! missing/foreign latest ref fails the run.

use std::sync::Arc;

use runic_agent::MediaResolver;
use runic_foundry::ArtifactResolver;
use runic_provider::CompletionRequest;
use runic_substrate::{ArtifactSource, ArtifactStore, MemoryArtifactStore};
use runic_types::{ContentBlock, Message, MessageContent};

fn blocks(msg: &Message) -> &[ContentBlock] {
    match &msg.content {
        MessageContent::Blocks(b) => b,
        MessageContent::Text(_) => &[],
    }
}

fn request(messages: Vec<Message>) -> CompletionRequest {
    CompletionRequest {
        model: "m".into(),
        messages,
        tools: vec![],
        max_tokens: 64,
        temperature: 0.0,
        system: None,
        thinking: None,
    }
}

async fn seed(store: &Arc<MemoryArtifactStore>, mime: &str, bytes: &[u8]) -> String {
    store
        .put("acme", "thread1", mime, ArtifactSource::UserUpload, bytes)
        .await
        .unwrap()
        .id
}

fn resolver(store: Arc<MemoryArtifactStore>) -> ArtifactResolver {
    ArtifactResolver::new(store as Arc<dyn ArtifactStore>, "acme", "thread1")
}

#[tokio::test]
async fn latest_ref_inlines_with_stored_mime_not_the_ref_payload() {
    let store = Arc::new(MemoryArtifactStore::new());
    // Stored as a PDF; the ref will LIE that it's an image.
    let id = seed(&store, "application/pdf", b"%PDF-1.7").await;

    let mut req = request(vec![Message::user_with_blocks(vec![
        ContentBlock::Text {
            text: "see".into(),
            provider_metadata: None,
        },
        ContentBlock::ArtifactRef {
            id,
            media_type: "image/png".into(), // a lie — must be ignored
            filename: Some("doc.pdf".into()),
        },
    ])]);

    resolver(store).resolve(&mut req).await.unwrap();

    let b = blocks(&req.messages[0]);
    // Resolved from the STORED mime (pdf → File), not the ref's claimed image.
    assert!(matches!(&b[1], ContentBlock::File { media_type, data }
        if media_type == "application/pdf" && !data.is_empty()));
    assert!(
        !b.iter()
            .any(|c| matches!(c, ContentBlock::ArtifactRef { .. }))
    );
}

#[tokio::test]
async fn older_ref_becomes_a_reminder() {
    let store = Arc::new(MemoryArtifactStore::new());
    let id = seed(&store, "application/pdf", b"%PDF-1.7").await;

    let mut req = request(vec![
        Message::user_with_blocks(vec![ContentBlock::ArtifactRef {
            id,
            media_type: "application/pdf".into(),
            filename: Some("invoice.pdf".into()),
        }]),
        Message::assistant("ok"),
        Message::user("and now?"),
    ]);

    resolver(store).resolve(&mut req).await.unwrap();

    match &blocks(&req.messages[0])[0] {
        ContentBlock::Text { text, .. } => {
            assert!(text.contains("previously uploaded"));
            assert!(text.contains("invoice.pdf"));
            assert!(text.contains("read_thread_artifact"));
        }
        other => panic!("expected reminder, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_latest_ref_fails_the_run() {
    let store = Arc::new(MemoryArtifactStore::new());
    // A real artifact exists, but the message references a different id.
    seed(&store, "image/png", b"real").await;

    let mut req = request(vec![Message::user_with_blocks(vec![
        ContentBlock::ArtifactRef {
            id: "art-foreign".into(),
            media_type: "image/png".into(),
            filename: None,
        },
    ])]);

    let err = resolver(store).resolve(&mut req).await.unwrap_err();
    assert!(err.contains("not available"));
}
