use runic::Input;
use runic::types::{ContentBlock, Message, MessageContent};
use serde::Deserialize;

use crate::error::ServeError;
use crate::routes::artifacts::MAX_ARTIFACT_BYTES;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct RunMessageRequest {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    #[schema(value_type = Option<Vec<Object>>)]
    pub content: Option<Vec<ContentBlock>>,
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub context: Option<serde_json::Value>,
    #[serde(default)]
    pub hook: Option<String>,
}

impl RunMessageRequest {
    pub fn into_message(self) -> Result<Message, ServeError> {
        match (self.content, self.message) {
            (Some(blocks), _) if !blocks.is_empty() => Ok(Message::user_with_blocks(blocks)),
            (_, Some(text)) if !text.trim().is_empty() => Ok(Message::user(text)),
            _ => Err(ServeError::BadRequest(
                "run request needs a non-empty `message` string or a non-empty `content` array"
                    .into(),
            )),
        }
    }
}

enum Incoming {
    Bytes(Vec<u8>),
    Stored(String),
}

fn incoming(source: &runic::types::Source) -> Result<Incoming, ServeError> {
    match source {
        runic::types::Source::Inline(bytes) if bytes.len() > MAX_ARTIFACT_BYTES => Err(
            ServeError::BadRequest("inline media exceeds size limit".into()),
        ),
        runic::types::Source::Inline(bytes) => Ok(Incoming::Bytes(bytes.clone())),
        runic::types::Source::Stored(id) => Ok(Incoming::Stored(id.clone())),
        _ => Err(ServeError::BadRequest(
            "a run turn carries inline media or a stored artifact id, not a provider reference"
                .into(),
        )),
    }
}

pub fn input_from_message(msg: Message) -> Result<Input, ServeError> {
    let blocks = match msg.content {
        MessageContent::Text(text) => return Ok(Input::text(text)),
        MessageContent::Blocks(blocks) => blocks,
    };

    let mut text: Option<String> = None;
    let mut attachments = Vec::new();
    for block in blocks {
        let (media_type, filename, source, is_image) = match block {
            ContentBlock::Text { text: part, .. } => {
                match &mut text {
                    Some(joined) => {
                        joined.push('\n');
                        joined.push_str(&part);
                    }
                    None => text = Some(part),
                }
                continue;
            }
            ContentBlock::Image {
                media_type,
                filename,
                source,
            } => (media_type, filename, source, true),
            ContentBlock::File {
                media_type,
                filename,
                source,
            } => (media_type, filename, source, false),
            _ => {
                return Err(ServeError::BadRequest(
                    "a run turn carries text, inline media and stored artifacts only".into(),
                ));
            }
        };

        attachments.push(match incoming(&source)? {
            Incoming::Bytes(bytes) if is_image => Attachment::Image { media_type, bytes },
            Incoming::Bytes(bytes) => Attachment::File { media_type, bytes },
            Incoming::Stored(id) => Attachment::Stored {
                id,
                media_type,
                filename,
            },
        });
    }

    let mut input = match text {
        Some(text) => Input::text(text),
        None => Input::new(),
    };
    for attachment in attachments {
        input = match attachment {
            Attachment::Image { media_type, bytes } => input.image(media_type, bytes),
            Attachment::File { media_type, bytes } => input.file(media_type, bytes),
            Attachment::Stored {
                id,
                media_type,
                filename: Some(filename),
            } => input.named_artifact(id, media_type, filename),
            Attachment::Stored {
                id,
                media_type,
                filename: None,
            } => input.artifact(id, media_type),
        };
    }
    Ok(input)
}

enum Attachment {
    Image {
        media_type: String,
        bytes: Vec<u8>,
    },
    File {
        media_type: String,
        bytes: Vec<u8>,
    },
    Stored {
        id: String,
        media_type: String,
        filename: Option<String>,
    },
}

pub fn with_context(input: Input, context: &serde_json::Value) -> Input {
    match context {
        serde_json::Value::Object(map) => input.config(map.clone()),
        _ => input,
    }
}
