use std::sync::Arc;

use runic::builtin::{ArtifactResolver, SearchChats};
use runic::hook::{HookOutcome, WriteHook};
use runic::tool::{Tool, ToolContext};
use runic_provider::CompletionRequest;
use runic_state::AgentState;
use runic_store::artifacts::{ArtifactSource, ArtifactStore};
use runic_store::{SessionEvent, Store};
use runic_types::{ContentBlock, Message, MessageContent, Role, Source};

fn user_said(text: &str) -> SessionEvent {
    SessionEvent::Message {
        run_id: "r".into(),
        msg: Message::user(text),
        at: chrono::Utc::now(),
    }
}

fn stored(id: &str, media_type: &str, filename: Option<&str>) -> Message {
    let source = Source::Stored(id.into());
    let filename = filename.map(str::to_string);
    let attachment = match media_type.starts_with("image/") {
        true => ContentBlock::Image {
            media_type: media_type.into(),
            filename,
            source,
        },
        false => ContentBlock::File {
            media_type: media_type.into(),
            filename,
            source,
        },
    };
    Message {
        role: Role::User,
        content: MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "look at this".into(),
                provider_metadata: None,
            },
            attachment,
        ]),
        ..Message::user("")
    }
}

fn request(messages: Vec<Message>) -> CompletionRequest {
    CompletionRequest {
        model: "m".into(),
        messages,
        tools: Vec::new(),
        max_tokens: 1024,
        temperature: 0.0,
        system: None,
        thinking: None,
    }
}

fn blocks(message: &Message) -> &[ContentBlock] {
    match &message.content {
        MessageContent::Blocks(blocks) => blocks,
        MessageContent::Text(_) => &[],
    }
}

fn source_of(block: &ContentBlock) -> Option<&Source> {
    match block {
        ContentBlock::Image { source, .. } | ContentBlock::File { source, .. } => Some(source),
        _ => None,
    }
}

async fn store_with_artifact(bytes: &[u8], mime: &str) -> (Store, String) {
    let store = Store::memory().unwrap();
    let artifact = store
        .artifacts()
        .put("tenant", "t1", mime, ArtifactSource::UserUpload, bytes)
        .await
        .unwrap();
    (store, artifact.id)
}

async fn resolve_once(hook: &ArtifactResolver, id: &str, mime: &str) -> ContentBlock {
    let mut req = request(vec![stored(id, mime, Some("report.pdf"))]);
    let mut state = AgentState::new("tenant", "t1", "");
    hook.before_model(&mut state, &mut req).await;
    blocks(&req.messages[0])[1].clone()
}

#[tokio::test]
async fn a_stored_document_arrives_at_the_provider_as_bytes() {
    let (store, id) = store_with_artifact(b"%PDF-1.7", "application/pdf").await;
    let hook = ArtifactResolver::new(store.artifacts());

    match resolve_once(&hook, &id, "application/pdf").await {
        ContentBlock::File {
            media_type,
            filename,
            source,
        } => {
            assert_eq!(media_type, "application/pdf");
            assert_eq!(filename.as_deref(), Some("report.pdf"));
            assert_eq!(source.inline(), Some(b"%PDF-1.7".as_slice()));
        }
        other => panic!("expected an inlined File, got {other:?}"),
    }
}

struct Presigning(Arc<dyn ArtifactStore>);

#[async_trait::async_trait]
impl ArtifactStore for Presigning {
    async fn put(
        &self,
        tenant: &str,
        session_id: &str,
        mime_type: &str,
        source: ArtifactSource,
        bytes: &[u8],
    ) -> runic_store::Result<runic_store::artifacts::Artifact> {
        self.0
            .put(tenant, session_id, mime_type, source, bytes)
            .await
    }

    async fn get(&self, _id: &str) -> runic_store::Result<Vec<u8>> {
        panic!("a backend that presigns must never be asked for the bytes");
    }

    async fn head(&self, id: &str) -> runic_store::Result<runic_store::artifacts::Artifact> {
        self.0.head(id).await
    }

    async fn list(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> runic_store::Result<Vec<runic_store::artifacts::Artifact>> {
        self.0.list(tenant, session_id).await
    }

    async fn delete(&self, id: &str) -> runic_store::Result<()> {
        self.0.delete(id).await
    }

    async fn url(&self, id: &str) -> runic_store::Result<Option<String>> {
        Ok(Some(format!("https://bucket.test/{id}?sig=abc")))
    }
}

#[tokio::test]
async fn a_backend_that_presigns_hands_over_a_url_and_never_reads_the_bytes() {
    let (store, id) = store_with_artifact(b"%PDF-1.7", "application/pdf").await;
    let hook = ArtifactResolver::new(Arc::new(Presigning(store.artifacts())));

    let block = resolve_once(&hook, &id, "application/pdf").await;
    assert_eq!(
        source_of(&block).and_then(Source::url),
        Some(format!("https://bucket.test/{id}?sig=abc").as_str())
    );
}

#[tokio::test]
async fn an_image_stays_an_image_block() {
    let (store, id) = store_with_artifact(b"\x89PNG", "image/png").await;
    let hook = ArtifactResolver::new(store.artifacts());

    assert!(matches!(
        resolve_once(&hook, &id, "image/png").await,
        ContentBlock::Image { .. }
    ));
}

#[tokio::test]
async fn no_stored_pointer_survives_the_hook() {
    let (store, id) = store_with_artifact(b"data", "application/pdf").await;
    let hook = ArtifactResolver::new(store.artifacts());

    let mut req = request(vec![
        stored(&id, "application/pdf", None),
        stored("art-does-not-exist", "application/pdf", Some("ghost.pdf")),
    ]);
    let mut state = AgentState::new("tenant", "t1", "");
    hook.before_model(&mut state, &mut req).await;

    let survivors = req
        .messages
        .iter()
        .flat_map(blocks)
        .filter_map(source_of)
        .filter(|source| source.stored().is_some())
        .count();
    assert_eq!(
        survivors, 0,
        "an unresolved pointer is a hard AgentError at the provider call, so none may remain"
    );
}

#[tokio::test]
async fn a_missing_artifact_degrades_to_text_rather_than_failing_the_turn() {
    let store = Store::memory().unwrap();
    let hook = ArtifactResolver::new(store.artifacts());

    match resolve_once(&hook, "art-ghost", "application/pdf").await {
        ContentBlock::Text { text, .. } => assert!(text.contains("art-ghost"), "{text}"),
        other => panic!("expected explanatory text, got {other:?}"),
    }
}

#[tokio::test]
async fn inline_and_uploaded_sources_are_left_alone() {
    let store = Store::memory().unwrap();
    let hook = ArtifactResolver::new(store.artifacts());

    let untouched = vec![
        Source::Inline(b"abc".to_vec()),
        Source::Url("https://example.test/a.png".into()),
        Source::Uploaded {
            file_id: "file-1".into(),
            provider: "anthropic".into(),
        },
    ];
    for source in untouched {
        let mut req = request(vec![Message::user_with_blocks(vec![ContentBlock::Image {
            media_type: "image/png".into(),
            filename: None,
            source: source.clone(),
        }])]);
        let mut state = AgentState::new("tenant", "t1", "");
        assert!(matches!(
            hook.before_model(&mut state, &mut req).await,
            HookOutcome::Noop
        ));
        assert_eq!(source_of(&blocks(&req.messages[0])[0]), Some(&source));
    }
}

#[tokio::test]
async fn a_request_with_no_artifacts_is_left_alone() {
    let store = Store::memory().unwrap();
    let hook = ArtifactResolver::new(store.artifacts());

    let mut req = request(vec![Message::user("plain text")]);
    let mut state = AgentState::new("tenant", "t1", "");
    assert!(matches!(
        hook.before_model(&mut state, &mut req).await,
        HookOutcome::Noop
    ));
}

#[tokio::test]
async fn search_chats_finds_another_session_and_skips_the_current_one() {
    let store = Store::memory().unwrap();
    store
        .sessions()
        .append(
            "tenant",
            "old",
            &user_said("the deploy key rotates monthly"),
        )
        .await
        .unwrap();
    store
        .sessions()
        .append(
            "tenant",
            "here",
            &user_said("the deploy key rotates monthly"),
        )
        .await
        .unwrap();

    let tool = SearchChats::new(store.sessions());
    let ctx = ToolContext::new("tenant", "here", "r");
    let out = tool
        .execute(serde_json::json!({ "query": "deploy key" }), &ctx)
        .await
        .unwrap();

    assert!(
        out.text().contains("old"),
        "found the earlier chat: {}",
        out.text()
    );
    assert!(
        !out.text().contains("session here"),
        "the current chat is already visible, so it is excluded: {}",
        out.text()
    );
}

#[tokio::test]
async fn search_chats_reports_nothing_found_rather_than_failing() {
    let store = Store::memory().unwrap();
    store
        .sessions()
        .append("tenant", "old", &user_said("something else entirely"))
        .await
        .unwrap();

    let tool = SearchChats::new(store.sessions());
    let ctx = ToolContext::new("tenant", "here", "r");
    let out = tool
        .execute(serde_json::json!({ "query": "kubernetes" }), &ctx)
        .await
        .unwrap();

    assert!(!out.is_error());
    assert!(out.text().contains("no earlier chat"));
}

#[tokio::test]
async fn search_chats_stays_inside_its_tenant() {
    let store = Store::memory().unwrap();
    store
        .sessions()
        .append(
            "other-tenant",
            "theirs",
            &user_said("the deploy key rotates"),
        )
        .await
        .unwrap();

    let tool = SearchChats::new(store.sessions());
    let ctx = ToolContext::new("tenant", "here", "r");
    let out = tool
        .execute(serde_json::json!({ "query": "deploy key" }), &ctx)
        .await
        .unwrap();

    assert!(
        out.text().contains("no earlier chat"),
        "another tenant's conversations are not searchable: {}",
        out.text()
    );
}

#[tokio::test]
async fn both_reach_the_agent_through_one_store() {
    let store = Store::memory()
        .unwrap()
        .tool_with(|store| SearchChats::new(store.sessions()))
        .hook_with(|store| ArtifactResolver::new(store.artifacts()));

    assert_eq!(store.tools().len(), 1);
    assert_eq!(store.tools()[0].name(), "search_chats");
    assert_eq!(store.hooks().len(), 1);
    assert_eq!(store.hooks()[0].name(), "artifact-resolver");
}
