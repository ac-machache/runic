use std::sync::Arc;

use base64::Engine;

use runic_agent::{CancelToken, RunContext};
use runic_provider::Provider;
use runic_state::Emitter;
use runic_types::{ContentBlock, Message};

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

    pub fn image(mut self, media_type: impl Into<String>, bytes: impl AsRef<[u8]>) -> Self {
        self.attachments.push(ContentBlock::Image {
            media_type: media_type.into(),
            data: encode(bytes),
        });
        self
    }

    pub fn file(mut self, media_type: impl Into<String>, bytes: impl AsRef<[u8]>) -> Self {
        self.attachments.push(ContentBlock::File {
            media_type: media_type.into(),
            data: encode(bytes),
        });
        self
    }

    pub fn artifact(mut self, id: impl Into<String>, media_type: impl Into<String>) -> Self {
        self.attachments.push(ContentBlock::ArtifactRef {
            id: id.into(),
            media_type: media_type.into(),
            filename: None,
        });
        self
    }

    pub fn named_artifact(
        mut self,
        id: impl Into<String>,
        media_type: impl Into<String>,
        filename: impl Into<String>,
    ) -> Self {
        self.attachments.push(ContentBlock::ArtifactRef {
            id: id.into(),
            media_type: media_type.into(),
            filename: Some(filename.into()),
        });
        self
    }

    pub fn events(mut self, events: Arc<dyn Emitter>) -> Self {
        self.ctx.events = Some(events);
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

fn encode(bytes: impl AsRef<[u8]>) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
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
            ContentBlock::Image { media_type, data }
                if media_type == "image/png" && data == "iVBORw=="
        ));
    }

    #[test]
    fn an_attachment_needs_no_text() {
        let (msg, _) = Input::new().artifact("art-7f3", "image/png").split();
        let MessageContent::Blocks(blocks) = msg.content else {
            panic!("attachments make a blocks message");
        };
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::ArtifactRef { id, .. } if id == "art-7f3"));
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
