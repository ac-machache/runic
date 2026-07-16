use serde::{Deserialize, Serialize};

pub const MAX_PROVENANCE_SOURCES: usize = 16;
pub const MAX_PROVENANCE_ID_CHARS: usize = 128;
pub const MAX_PROVENANCE_SOURCE_CHARS: usize = 2048;
pub const MAX_PROVENANCE_TITLE_CHARS: usize = 256;
pub const MAX_PROVENANCE_SNIPPET_CHARS: usize = 1024;
pub const MAX_PROVENANCE_METADATA_BYTES: usize = 2048;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceSource {
    pub id: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl ProvenanceSource {
    pub fn new(id: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            source: source.into(),
            title: None,
            snippet: None,
            metadata: None,
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn with_snippet(mut self, snippet: impl Into<String>) -> Self {
        self.snippet = Some(snippet.into());
        self
    }

    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn sanitized(mut self) -> Self {
        self.id = truncate_chars(&self.id, MAX_PROVENANCE_ID_CHARS);
        self.source = truncate_chars(&sanitize_source(&self.source), MAX_PROVENANCE_SOURCE_CHARS);
        self.title = self
            .title
            .map(|title| truncate_chars(&title, MAX_PROVENANCE_TITLE_CHARS));
        self.snippet = self
            .snippet
            .map(|snippet| truncate_chars(&snippet, MAX_PROVENANCE_SNIPPET_CHARS));
        self.metadata = self
            .metadata
            .map(|mut meta| {
                scrub_metadata(&mut meta);
                meta
            })
            .filter(|meta| {
                serde_json::to_vec(meta)
                    .map(|bytes| bytes.len() <= MAX_PROVENANCE_METADATA_BYTES)
                    .unwrap_or(false)
            });
        self
    }
}

fn scrub_metadata(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|key, _| !is_secret_param(key));
            for inner in map.values_mut() {
                scrub_metadata(inner);
            }
        }
        serde_json::Value::Array(items) => {
            for inner in items {
                scrub_metadata(inner);
            }
        }
        _ => {}
    }
}

pub fn sanitize_provenance(sources: Vec<ProvenanceSource>) -> Vec<ProvenanceSource> {
    sources
        .into_iter()
        .take(MAX_PROVENANCE_SOURCES)
        .map(ProvenanceSource::sanitized)
        .collect()
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => text[..byte_idx].to_string(),
        None => text.to_string(),
    }
}

fn has_scheme_ci(source: &str, scheme: &str) -> bool {
    source.len() >= scheme.len()
        && source.as_bytes()[..scheme.len()].eq_ignore_ascii_case(scheme.as_bytes())
}

fn sanitize_source(source: &str) -> String {
    if has_scheme_ci(source, "file://") {
        return redact_local_path(&source["file://".len()..]);
    }
    if source.starts_with('/')
        || source.starts_with("~/")
        || source.starts_with("\\\\")
        || is_windows_drive_path(source)
    {
        return redact_local_path(source);
    }
    if has_scheme_ci(source, "http://") || has_scheme_ci(source, "https://") {
        return sanitize_url(source);
    }
    source.to_string()
}

fn is_windows_drive_path(source: &str) -> bool {
    let bytes = source.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn redact_local_path(path: &str) -> String {
    let basename = path
        .rsplit(['/', '\\'])
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or("(root)");
    format!("file:{basename}")
}

fn sanitize_url(url_str: &str) -> String {
    let Ok(mut url) = url::Url::parse(url_str) else {
        return "[unparseable-url]".to_string();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| !is_secret_param(name))
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept);
    }
    url.to_string()
}

fn is_secret_param(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    matches!(
        lowered.as_str(),
        "sig"
            | "signature"
            | "token"
            | "access_token"
            | "id_token"
            | "apikey"
            | "api_key"
            | "key"
            | "password"
            | "secret"
            | "auth"
            | "authorization"
            | "credential"
            | "credentials"
            | "sas"
    ) || lowered.starts_with("x-amz-")
        || lowered.starts_with("x-goog-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_lose_userinfo_and_signed_params_but_keep_the_rest() {
        let source = ProvenanceSource::new(
            "s1",
            "https://user:hunter2@example.com/docs?page=2&X-Amz-Signature=abc&token=zzz#frag",
        )
        .sanitized();
        assert_eq!(source.source, "https://example.com/docs?page=2#frag");

        let clean = ProvenanceSource::new("s2", "https://example.com/a?q=rust").sanitized();
        assert_eq!(clean.source, "https://example.com/a?q=rust");

        let all_secret = ProvenanceSource::new("s3", "https://example.com/a?sig=x").sanitized();
        assert_eq!(all_secret.source, "https://example.com/a");
    }

    #[test]
    fn sanitization_survives_case_and_percent_encoding_tricks() {
        let upper =
            ProvenanceSource::new("s1", "HTTPS://user:pw@Example.com/a?Token=zzz").sanitized();
        assert!(!upper.source.contains("user"));
        assert!(!upper.source.contains("pw"));
        assert!(!upper.source.to_ascii_lowercase().contains("token"));

        let encoded =
            ProvenanceSource::new("s2", "https://example.com/a?%74oken=secret&page=2").sanitized();
        assert!(!encoded.source.contains("secret"));
        assert!(encoded.source.contains("page=2"));

        let upper_file = ProvenanceSource::new("s3", "FILE:///etc/passwd").sanitized();
        assert_eq!(upper_file.source, "file:passwd");
    }

    #[test]
    fn unparseable_urls_fail_closed() {
        let broken = ProvenanceSource::new("s", "http://exa mple.com/?token=x").sanitized();
        assert_eq!(broken.source, "[unparseable-url]");
    }

    #[test]
    fn non_ascii_sources_do_not_panic_the_scheme_check() {
        let source = ProvenanceSource::new("s", "日本語データベース記事への参照").sanitized();
        assert_eq!(source.source, "日本語データベース記事への参照");
    }

    #[test]
    fn secret_keys_are_scrubbed_from_metadata_recursively() {
        let source = ProvenanceSource::new("s", "https://e.com")
            .with_metadata(serde_json::json!({
                "token": "secret",
                "rank": 1,
                "nested": { "api_key": "x", "ok": 2 },
                "list": [{ "password": "y", "keep": 3 }]
            }))
            .sanitized();
        let meta = source.metadata.unwrap();
        assert_eq!(
            meta,
            serde_json::json!({
                "rank": 1,
                "nested": { "ok": 2 },
                "list": [{ "keep": 3 }]
            })
        );
    }

    #[test]
    fn local_paths_are_redacted_to_basenames() {
        for (raw, expected) in [
            ("/Users/alice/secrets/report.pdf", "file:report.pdf"),
            ("file:///etc/passwd", "file:passwd"),
            ("~/notes/todo.md", "file:todo.md"),
            ("C:\\Users\\alice\\doc.txt", "file:doc.txt"),
        ] {
            let source = ProvenanceSource::new("s", raw).sanitized();
            assert_eq!(source.source, expected, "raw: {raw}");
        }
        let artifact = ProvenanceSource::new("s", "artifact:abc-123").sanitized();
        assert_eq!(artifact.source, "artifact:abc-123");
    }

    #[test]
    fn oversized_fields_truncate_deterministically_and_counts_are_bounded() {
        let big = "é".repeat(MAX_PROVENANCE_SNIPPET_CHARS + 50);
        let source = ProvenanceSource::new("s", "https://e.com")
            .with_title("t".repeat(500))
            .with_snippet(big.clone())
            .sanitized();
        assert_eq!(
            source.title.as_ref().unwrap().chars().count(),
            MAX_PROVENANCE_TITLE_CHARS
        );
        assert_eq!(
            source.snippet.as_ref().unwrap().chars().count(),
            MAX_PROVENANCE_SNIPPET_CHARS
        );
        let again = ProvenanceSource::new("s", "https://e.com")
            .with_snippet(big)
            .sanitized();
        assert_eq!(source.snippet, again.snippet);

        let many = (0..40)
            .map(|i| ProvenanceSource::new(format!("s{i}"), "https://e.com"))
            .collect();
        assert_eq!(sanitize_provenance(many).len(), MAX_PROVENANCE_SOURCES);
    }

    #[test]
    fn oversized_metadata_is_dropped_not_truncated() {
        let source = ProvenanceSource::new("s", "https://e.com")
            .with_metadata(serde_json::json!({ "blob": "x".repeat(4000) }))
            .sanitized();
        assert!(source.metadata.is_none());

        let small = ProvenanceSource::new("s", "https://e.com")
            .with_metadata(serde_json::json!({ "rank": 1 }))
            .sanitized();
        assert_eq!(small.metadata.unwrap()["rank"], 1);
    }
}
