//! Resolving an `artifact_ref` into bytes is a `before_model` hook's job. The
//! loop's only remaining stake in it: a pointer must never reach a provider, so
//! an unresolved one fails the run instead of quietly dropping the file.

mod harness;

use std::sync::Arc;

use harness::*;
use runic_agent::{AgentError, Runner};
use runic_types::{ContentBlock, Message};

fn ref_message(id: &str) -> Message {
    Message::user_with_blocks(vec![
        ContentBlock::Text {
            text: "what is in this image".into(),
            provider_metadata: None,
        },
        ContentBlock::ArtifactRef {
            id: id.into(),
            media_type: "image/png".into(),
            filename: Some("p.png".into()),
        },
    ])
}

#[tokio::test]
async fn an_unresolved_artifact_ref_fails_the_run_naming_the_id() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("never")]));
    let mut agent = Runner::builder(provider.clone(), "t", "s")
        .model("test")
        .build();

    let err = agent
        .run_message(ref_message("art-7f3"))
        .await
        .expect_err("no hook resolved the ref, so the call must fail");

    match err {
        AgentError::Media(message) => assert!(
            message.contains("art-7f3"),
            "the error names the artifact that went missing: {message}"
        ),
        other => panic!("expected a media error, got {other:?}"),
    }
    assert_eq!(
        provider.call_count(),
        0,
        "the pointer must never reach the provider"
    );
}
