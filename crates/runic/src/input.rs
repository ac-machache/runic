use std::sync::Arc;

use runic_agent::{CancelToken, RunContext};
use runic_provider::Provider;
use runic_state::Emitter;
use runic_types::{ContentBlock, Message, Source};

#[derive(Default)]
pub struct Input {
    text: Option<String>,
    attachments: Vec<ContentBlock>,
    ctx: RunContext,
}

impl Input {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }

    pub fn image(mut self, media_type: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        self.attachments.push(ContentBlock::Image {
            media_type: media_type.into(),
            filename: None,
            source: Source::Inline(bytes.into()),
        });
        self
    }

    pub fn file(mut self, media_type: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        self.attachments.push(ContentBlock::File {
            media_type: media_type.into(),
            filename: None,
            source: Source::Inline(bytes.into()),
        });
        self
    }

    pub fn artifact(self, id: impl Into<String>, media_type: impl Into<String>) -> Self {
        self.stored(id, media_type, None)
    }

    pub fn named_artifact(
        self,
        id: impl Into<String>,
        media_type: impl Into<String>,
        filename: impl Into<String>,
    ) -> Self {
        self.stored(id, media_type, Some(filename.into()))
    }

    fn stored(
        mut self,
        id: impl Into<String>,
        media_type: impl Into<String>,
        filename: Option<String>,
    ) -> Self {
        let media_type = media_type.into();
        let source = Source::Stored(id.into());
        self.attachments
            .push(match media_type.starts_with("image/") {
                true => ContentBlock::Image {
                    media_type,
                    filename,
                    source,
                },
                false => ContentBlock::File {
                    media_type,
                    filename,
                    source,
                },
            });
        self
    }

    pub fn events(mut self, events: Arc<dyn Emitter>) -> Self {
        self.ctx.events.push(events);
        self
    }

    pub fn cancel(mut self, cancel: CancelToken) -> Self {
        self.ctx.cancel = Some(cancel);
        self
    }

    pub fn steering(mut self, steering: tokio::sync::mpsc::UnboundedReceiver<String>) -> Self {
        self.ctx.steering = Some(steering);
        self
    }

    pub fn answer(mut self, answer: serde_json::Value) -> Self {
        self.ctx.answer = Some(answer);
        self
    }

    pub fn run_id(mut self, run_id: impl Into<String>) -> Self {
        self.ctx.run_id = Some(run_id.into());
        self
    }

    pub fn actor(mut self, actor: impl Into<String>) -> Self {
        self.ctx.actor = Some(actor.into());
        self
    }

    pub fn agent_name(mut self, agent: impl Into<String>) -> Self {
        self.ctx.agent = Some(agent.into());
        self
    }

    pub fn mode(mut self, mode: &'static str) -> Self {
        self.ctx.mode = Some(mode);
        self
    }

    pub fn provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.ctx.provider = Some(provider);
        self
    }

    pub fn config(mut self, config: serde_json::Map<String, serde_json::Value>) -> Self {
        self.ctx.config = config;
        self
    }

    pub fn config_value(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.ctx.config.insert(key.into(), value);
        self
    }

    pub(crate) fn split(self) -> (Message, RunContext) {
        let Self {
            text,
            attachments,
            ctx,
        } = self;
        if attachments.is_empty() {
            return (Message::user(text.unwrap_or_default()), ctx);
        }
        let mut blocks = Vec::with_capacity(attachments.len() + 1);
        if let Some(text) = text {
            blocks.push(ContentBlock::Text {
                text,
                provider_metadata: None,
            });
        }
        blocks.extend(attachments);
        (Message::user_with_blocks(blocks), ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_types::MessageContent;

    #[test]
    fn text_alone_stays_a_plain_message() {
        let (msg, _) = Input::text("hello").split();
        assert!(matches!(msg.content, MessageContent::Text(text) if text == "hello"));
    }

    #[test]
    fn an_attachment_puts_the_text_first_and_encodes_the_bytes() {
        let (msg, _) = Input::text("what is this")
            .image("image/png", b"\x89PNG")
            .split();
        let MessageContent::Blocks(blocks) = msg.content else {
            panic!("attachments make a blocks message");
        };
        assert!(matches!(&blocks[0], ContentBlock::Text { text, .. } if text == "what is this"));
        assert!(matches!(
            &blocks[1],
            ContentBlock::Image { media_type, source, .. }
                if media_type == "image/png" && source.inline() == Some(b"\x89PNG".as_slice())
        ));
    }

    #[test]
    fn an_attachment_needs_no_text() {
        let (msg, _) = Input::new().artifact("art-7f3", "image/png").split();
        let MessageContent::Blocks(blocks) = msg.content else {
            panic!("attachments make a blocks message");
        };
        assert_eq!(blocks.len(), 1);
        assert!(matches!(
            &blocks[0],
            ContentBlock::Image { source, .. } if source.stored() == Some("art-7f3")
        ));
    }

    #[test]
    fn a_stored_document_is_a_file_block_not_an_image() {
        let (msg, _) = Input::new()
            .named_artifact("art-9", "application/pdf", "invoice.pdf")
            .split();
        let MessageContent::Blocks(blocks) = msg.content else {
            panic!("attachments make a blocks message");
        };
        assert!(matches!(
            &blocks[0],
            ContentBlock::File { filename, source, .. }
                if filename.as_deref() == Some("invoice.pdf") && source.stored() == Some("art-9")
        ));
    }

    #[test]
    fn the_run_knobs_ride_along_untouched() {
        let cancel = CancelToken::new();
        let (_, ctx) = Input::text("go")
            .cancel(cancel.clone())
            .run_id("r-1")
            .actor("user-42")
            .mode("stream")
            .config_value("locale", serde_json::json!("fr"))
            .split();

        assert!(ctx.cancel.expect("a cancel token").is_same(&cancel));
        assert_eq!(ctx.run_id.as_deref(), Some("r-1"));
        assert_eq!(ctx.actor.as_deref(), Some("user-42"));
        assert_eq!(ctx.mode, Some("stream"));
        assert_eq!(ctx.config["locale"], "fr");
    }
}
