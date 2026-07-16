use async_trait::async_trait;

pub const SPILL_PREVIEW_CHARS: usize = 256;

#[derive(Debug, Clone)]
pub struct SpilledArtifact {
    pub id: String,
    pub mime: String,
    pub size: u64,
}

#[async_trait]
pub trait ToolOutputSpill: Send + Sync {
    async fn store(
        &self,
        tenant: &str,
        session: &str,
        mime: &str,
        bytes: &[u8],
    ) -> anyhow::Result<SpilledArtifact>;
}

pub(crate) fn serialize_output(output: &serde_json::Value) -> (String, &'static str) {
    match output {
        serde_json::Value::String(text) => (text.clone(), "text/plain"),
        other => (other.to_string(), "application/json"),
    }
}

pub(crate) fn truncate_to_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut idx = max_bytes;
    while !text.is_char_boundary(idx) {
        idx -= 1;
    }
    &text[..idx]
}

pub(crate) fn preview_of(text: &str) -> String {
    match text.char_indices().nth(SPILL_PREVIEW_CHARS) {
        Some((byte_idx, _)) => format!("{}…", &text[..byte_idx]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialization_is_utf8_bytes_of_string_or_compact_json() {
        let (text, mime) = serialize_output(&serde_json::json!("plain"));
        assert_eq!((text.as_str(), mime), ("plain", "text/plain"));

        let (text, mime) = serialize_output(&serde_json::json!({ "a": [1, 2] }));
        assert_eq!(
            (text.as_str(), mime),
            (r#"{"a":[1,2]}"#, "application/json")
        );
    }

    #[test]
    fn previews_truncate_on_char_boundaries() {
        let short = preview_of("tiny");
        assert_eq!(short, "tiny");

        let long = "é".repeat(SPILL_PREVIEW_CHARS + 10);
        let preview = preview_of(&long);
        assert_eq!(preview.chars().count(), SPILL_PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
    }
}
