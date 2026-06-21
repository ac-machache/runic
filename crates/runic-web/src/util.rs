//! Small pure helpers shared across the event and view layers.

use serde_json::Value;

use crate::model::Source;

pub fn clean_tool_name(n: &str) -> String {
    if let Some(rest) = n.strip_prefix("mcp__") {
        let parts: Vec<&str> = rest.splitn(2, "__").collect();
        if parts.len() == 2 {
            return format!("{} · {}", parts[0], parts[1]);
        }
    }
    n.to_string()
}

pub fn pretty_json(v: &Value) -> String {
    truncate(&serde_json::to_string_pretty(v).unwrap_or_default(), 600)
}

pub fn short_id(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…", &id[..12])
    } else {
        id.to_string()
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

pub fn md_to_html(src: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(src, opts);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// Normalize a message's `content` into a block list. The new `MessageContent`
/// serializes simple text as a bare string (`"content": "hi"`) and structured
/// messages as an array of `{type,…}` blocks — this collapses both into the
/// block form the UI parses.
pub fn content_blocks(msg: &Value) -> Vec<Value> {
    match msg.get("content") {
        Some(Value::String(s)) => vec![serde_json::json!({ "type": "text", "text": s })],
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    }
}

/// First text block of a message (for de-duping user/assistant rows).
pub fn first_text(msg: &Value) -> Option<String> {
    content_blocks(msg).iter().find_map(|b| {
        if b.get("type").and_then(|v| v.as_str()) == Some("text") {
            b.get("text")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        } else {
            None
        }
    })
}

/// Grounding sources from a tool result's `metadata.sources` (web search etc.).
pub fn parse_sources(metadata: Option<&Value>) -> Vec<Source> {
    let Some(arr) = metadata
        .and_then(|m| m.get("sources"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|s| {
            let url = s.get("url").and_then(|v| v.as_str())?.to_string();
            let title = s
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(Source { title, url })
        })
        .collect()
}

/// One-line summary of a content block, for the State tab's message list.
pub fn render_block_summary(b: &Value) -> String {
    match b.get("type").and_then(|v| v.as_str()) {
        Some("text") => b
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Some("tool_use") => format!(
            "→ tool_use {}({})",
            b.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
            b.get("input")
                .map(|v| truncate(&v.to_string(), 120))
                .unwrap_or_default()
        ),
        Some("tool_result") => format!(
            "← tool_result {}",
            truncate(b.get("content").and_then(|v| v.as_str()).unwrap_or(""), 120)
        ),
        Some("image") => "[image]".to_string(),
        Some("reasoning") => format!(
            "[thinking] {}",
            truncate(b.get("text").and_then(|v| v.as_str()).unwrap_or(""), 120)
        ),
        Some("blob") => "[blob]".to_string(),
        other => format!("[{}]", other.unwrap_or("?")),
    }
}
