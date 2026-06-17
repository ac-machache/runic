//! Custom tools for the `wikis_expert` sub-agent.
//!
//! The built-in shell tools (`read_file`, `ls`, `glob`, `grep`) operate
//! on the wiki mount fine, but two access patterns need bespoke tools:
//!
//! - [`GetPageContentTool`] — a wiki's `sources/<doc>.json` is the full
//!   page-by-page extraction of a PDF, often huge. `read_file` would dump
//!   the whole blob; this tool parses it and returns only the requested
//!   pages, formatted `[Page N]` with the referenced image filenames.
//! - [`GetImageTool`] — returns an actual image the model can SEE, via a
//!   [`ToolResultImage`] (base64) folded into the tool result. Returning
//!   base64 as plain text would blow the context window and the model
//!   still couldn't look at it.
//!
//! Both read through an [`Arc<dyn StorageBackend>`] scoped to the same
//! wiki mount the sub-agent's shell tools see, so keys are relative to
//! the wiki root (`sources/<doc>.json`, `sources/images/<doc>/<img>`).
//! Port of coral's `wikis_expert/tools.py`, minus the GCS/org plumbing —
//! runic's isolated filesystem mount already scopes the surface.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use runic_storage_backend::{StorageBackend, StorageError};
use runic_tool_core::{Tool, ToolContext, ToolResult};
use runic_message_types::ToolResultImage;
use serde_json::Value;

/// Hard cap on pages materialised per call — a hostile `pages` spec like
/// `1-999999999` must not balloon the page set into an OOM.
const MAX_PAGES_PER_CALL: usize = 500;

/// Reject a single LLM-controlled path segment (`doc`, `image`). Mirrors
/// coral's `_is_safe_path_component`: no separators, no `..` escape, no
/// leading dot, not blank. Spaces and parens are allowed — real wiki
/// filenames carry them (e.g. `cipan 2026 final (1)`).
fn is_safe_segment(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    if value.contains('/') || value.contains('\\') || value.contains('\0') {
        return false;
    }
    if value.contains("..") {
        return false;
    }
    !value.starts_with('.')
}

/// Parse `"3-5,7,10-12"` into a sorted, de-duped `[3,4,5,7,10,11,12]`.
/// Malformed parts are skipped silently; positive pages only; the total
/// is capped at [`MAX_PAGES_PER_CALL`] (ranges are clamped before
/// materialising so a huge span can't allocate first and truncate after).
fn parse_pages(spec: &str) -> Vec<u32> {
    let mut out: BTreeSet<u32> = BTreeSet::new();
    for raw in spec.split(',') {
        if out.len() >= MAX_PAGES_PER_CALL {
            break;
        }
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((lo_s, hi_s)) = part.split_once('-') {
            let (Ok(lo), Ok(hi)) = (lo_s.trim().parse::<u32>(), hi_s.trim().parse::<u32>()) else {
                continue;
            };
            if hi < lo {
                continue;
            }
            let remaining = MAX_PAGES_PER_CALL - out.len();
            let span = (hi - lo + 1) as usize;
            let capped_hi = if span > remaining {
                lo + remaining as u32 - 1
            } else {
                hi
            };
            for n in lo..=capped_hi {
                if n > 0 {
                    out.insert(n);
                }
            }
        } else if let Ok(n) = part.parse::<u32>()
            && n > 0
        {
            out.insert(n);
        }
    }
    out.into_iter().collect()
}

/// MIME type from a filename extension, defaulting to PNG.
fn mime_for(image: &str) -> &'static str {
    let ext = image.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

// ── get_page_content ─────────────────────────────────────────────────

const GET_PAGE_CONTENT_DESC: &str = "Récupère le contenu texte de pages spécifiques d'un \
document indexé (le fichier `sources/<doc>.json`). Le built-in `read_file` dumperait tout le \
JSON ; ce tool extrait UNIQUEMENT les pages demandées. À utiliser pour les chiffres / le \
verbatim, pas pour parcourir un PDF en entier.";

pub struct GetPageContentTool {
    storage: Arc<dyn StorageBackend>,
}

impl GetPageContentTool {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl Tool for GetPageContentTool {
    fn name(&self) -> &str {
        "get_page_content"
    }

    fn description(&self) -> &str {
        GET_PAGE_CONTENT_DESC
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "doc": {
                    "type": "string",
                    "description": "Nom du document sans extension (ex: 'cipan 2026 final (1)'). \
                        Liste les documents via `ls sources/`."
                },
                "pages": {
                    "type": "string",
                    "description": "Spec de pages style '3-5,7,10-12' — pages ou intervalles séparés par virgule."
                }
            },
            "required": ["doc", "pages"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let doc = input.get("doc").and_then(Value::as_str).unwrap_or_default();
        let pages_spec = input.get("pages").and_then(Value::as_str).unwrap_or_default();

        if !is_safe_segment(doc) {
            return ToolResult::error(format!("Invalid doc name: {doc:?}"));
        }
        let requested = parse_pages(pages_spec);
        if requested.is_empty() {
            return ToolResult::error(format!("Invalid page spec: {pages_spec:?}"));
        }
        let wanted: BTreeSet<u32> = requested.iter().copied().collect();

        let key = format!("sources/{doc}.json");
        let raw = match self.storage.read(&key).await {
            Ok(bytes) => bytes,
            Err(StorageError::NotFound { .. }) => {
                return ToolResult::error(format!("Document not found: {doc}"));
            }
            Err(e) => return ToolResult::error(format!("Storage error reading {doc}: {e}")),
        };

        let data: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(e) => return ToolResult::error(format!("File is not valid JSON: {doc} ({e})")),
        };
        let Some(entries) = data.as_array() else {
            return ToolResult::error(format!("Unexpected schema in {doc} (expected a JSON list)."));
        };

        let mut blocks: Vec<String> = Vec::new();
        for entry in entries {
            let page = entry.get("page").and_then(Value::as_u64).map(|p| p as u32);
            let Some(page) = page else { continue };
            if !wanted.contains(&page) {
                continue;
            }
            let content = entry.get("content").and_then(Value::as_str).unwrap_or("");
            let mut block = format!("[Page {page}]\n{content}");

            if let Some(images) = entry.get("images").and_then(Value::as_array) {
                let names: Vec<&str> = images
                    .iter()
                    .filter_map(|img| img.get("path").and_then(Value::as_str))
                    .map(|p| p.rsplit('/').next().unwrap_or(p))
                    .collect();
                if !names.is_empty() {
                    block.push_str(&format!("\n[Images: {}]", names.join(", ")));
                }
            }
            blocks.push(block);
        }

        if blocks.is_empty() {
            return ToolResult::ok(format!("No content found for pages {pages_spec} in {doc}."));
        }
        ToolResult::ok(format!("{}\n", blocks.join("\n\n")))
    }
}

// ── get_image ────────────────────────────────────────────────────────

const GET_IMAGE_DESC: &str = "Affiche une image du wiki — le modèle la VOIT. Passe le nom de \
fichier (ex: 'p5_img14.jpg') tel que renvoyé dans la ligne `[Images: ...]` de `get_page_content`.";

pub struct GetImageTool {
    storage: Arc<dyn StorageBackend>,
}

impl GetImageTool {
    pub fn new(storage: Arc<dyn StorageBackend>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl Tool for GetImageTool {
    fn name(&self) -> &str {
        "get_image"
    }

    fn description(&self) -> &str {
        GET_IMAGE_DESC
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "doc": {
                    "type": "string",
                    "description": "Nom du document sans extension (ex: 'cipan 2026 final (1)')."
                },
                "image": {
                    "type": "string",
                    "description": "Nom de fichier image seul (ex: 'p5_img14.jpg'), tel que renvoyé par get_page_content."
                }
            },
            "required": ["doc", "image"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let doc = input.get("doc").and_then(Value::as_str).unwrap_or_default();
        let image = input.get("image").and_then(Value::as_str).unwrap_or_default();

        if !is_safe_segment(doc) || !is_safe_segment(image) {
            return ToolResult::error(format!("Invalid doc or image name: {doc:?} / {image:?}"));
        }

        let key = format!("sources/images/{doc}/{image}");
        let raw = match self.storage.read(&key).await {
            Ok(bytes) => bytes,
            Err(StorageError::NotFound { .. }) => {
                return ToolResult::error(format!("Image not found: {doc}/{image}"));
            }
            Err(e) => return ToolResult::error(format!("Storage error reading {image}: {e}")),
        };

        let data = base64::engine::general_purpose::STANDARD.encode(&raw);
        ToolResult::ok(format!("Image: {doc}/{image}")).with_images(vec![ToolResultImage {
            media_type: mime_for(image).to_string(),
            data,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_storage_backend::MemoryBackend;

    fn ctx() -> ToolContext {
        ToolContext::new("s".into(), "r".into(), 0, Default::default())
    }

    #[test]
    fn parse_pages_handles_ranges_singles_and_dups() {
        assert_eq!(parse_pages("3-5,7,10-12"), vec![3, 4, 5, 7, 10, 11, 12]);
        assert_eq!(parse_pages("5,5,5"), vec![5]);
        assert_eq!(parse_pages(" 2 , 1 "), vec![1, 2]);
        assert!(parse_pages("abc").is_empty());
        assert!(parse_pages("").is_empty());
        // reversed range skipped, page 0 dropped
        assert_eq!(parse_pages("9-3,0,4"), vec![4]);
    }

    #[test]
    fn parse_pages_caps_huge_ranges() {
        let pages = parse_pages("1-100000000");
        assert_eq!(pages.len(), MAX_PAGES_PER_CALL);
        assert_eq!(*pages.first().unwrap(), 1);
    }

    #[test]
    fn unsafe_segments_rejected() {
        assert!(!is_safe_segment(""));
        assert!(!is_safe_segment("  "));
        assert!(!is_safe_segment("../etc/passwd"));
        assert!(!is_safe_segment("a/b"));
        assert!(!is_safe_segment(".hidden"));
        assert!(is_safe_segment("cipan 2026 final (1)"));
        assert!(is_safe_segment("p5_img14.jpg"));
    }

    #[test]
    fn mime_detection() {
        assert_eq!(mime_for("a.jpg"), "image/jpeg");
        assert_eq!(mime_for("a.JPEG"), "image/jpeg");
        assert_eq!(mime_for("a.png"), "image/png");
        assert_eq!(mime_for("noext"), "image/png");
    }

    async fn store(key: &str, bytes: &[u8]) -> Arc<dyn StorageBackend> {
        let b: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        b.write(key, bytes).await.unwrap();
        b
    }

    #[tokio::test]
    async fn get_page_content_returns_requested_pages_with_images() {
        let json = serde_json::json!([
            {"page": 1, "content": "intro", "images": []},
            {"page": 2, "content": "body", "images": [{"path": "sources/images/doc/p2_img1.jpg"}]},
            {"page": 3, "content": "tail", "images": []},
        ])
        .to_string();
        let storage = store("sources/doc.json", json.as_bytes()).await;
        let tool = GetPageContentTool::new(storage);

        let r = tool
            .execute(serde_json::json!({"doc": "doc", "pages": "2-3"}), &ctx())
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("[Page 2]\nbody"));
        assert!(r.content.contains("[Images: p2_img1.jpg]"));
        assert!(r.content.contains("[Page 3]\ntail"));
        assert!(!r.content.contains("intro"));
    }

    #[tokio::test]
    async fn get_page_content_missing_doc_errors() {
        let tool = GetPageContentTool::new(store("sources/other.json", b"[]").await);
        let r = tool
            .execute(serde_json::json!({"doc": "doc", "pages": "1"}), &ctx())
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("not found"));
    }

    #[tokio::test]
    async fn get_image_returns_base64_image_block() {
        let bytes = b"\x89PNG\r\n\x1a\nfakepng";
        let storage = store("sources/images/doc/p1_img1.png", bytes).await;
        let tool = GetImageTool::new(storage);

        let r = tool
            .execute(
                serde_json::json!({"doc": "doc", "image": "p1_img1.png"}),
                &ctx(),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(r.images.len(), 1);
        assert_eq!(r.images[0].media_type, "image/png");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&r.images[0].data)
            .unwrap();
        assert_eq!(decoded, bytes);
    }

    #[tokio::test]
    async fn get_image_traversal_rejected() {
        let tool = GetImageTool::new(store("sources/images/doc/a.png", b"x").await);
        let r = tool
            .execute(
                serde_json::json!({"doc": "../../secret", "image": "a.png"}),
                &ctx(),
            )
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("Invalid"));
    }
}
