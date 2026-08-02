use base64::Engine;
use runic::Input;
use runic::types::{ContentBlock, Message, MessageContent};
use serde::Deserialize;

use crate::app::AppState;
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

fn decode(data: &str) -> Result<Vec<u8>, ServeError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|_| ServeError::BadRequest("invalid base64 in content block".into()))?;
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(ServeError::BadRequest(
            "inline media exceeds size limit".into(),
        ));
    }
    Ok(bytes)
}

pub async fn input_from_message(
    state: &AppState,
    tenant: &str,
    session_id: &str,
    msg: Message,
) -> Result<Input, ServeError> {
    let blocks = match msg.content {
        MessageContent::Text(text) => return Ok(Input::text(text)),
        MessageContent::Blocks(blocks) => blocks,
    };

    let has_ref = blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::ArtifactRef { .. }));
    let owned = match has_ref {
        true => state.artifacts().list(tenant, session_id).await?,
        false => Vec::new(),
    };

    let mut text: Option<String> = None;
    let mut attachments = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text: part, .. } => match &mut text {
                Some(joined) => {
                    joined.push('\n');
                    joined.push_str(&part);
                }
                None => text = Some(part),
            },
            ContentBlock::Image { media_type, data } => {
                attachments.push(Attachment::Image {
                    media_type,
                    bytes: decode(&data)?,
                });
            }
            ContentBlock::File { media_type, data } => {
                attachments.push(Attachment::File {
                    media_type,
                    bytes: decode(&data)?,
                });
            }
            ContentBlock::ArtifactRef { id, filename, .. } => {
                let Some(artifact) = owned.iter().find(|owned| owned.id == id) else {
                    return Err(ServeError::BadRequest(
                        "artifact_ref does not belong to this session".into(),
                    ));
                };
                attachments.push(Attachment::Stored {
                    id,
                    media_type: artifact.mime_type.clone(),
                    filename,
                });
            }
            _ => {
                return Err(ServeError::BadRequest(
                    "a run turn carries text, inline media and artifact references only".into(),
                ));
            }
        }
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
