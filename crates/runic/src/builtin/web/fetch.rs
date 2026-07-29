use runic_macros::tool;
use runic_tool::{ToolContext, ToolResult};

use super::client::WebClient;
use super::text::html_to_text;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    Raw,
    #[default]
    Markdown,
}

pub const DEFAULT_MAX_LENGTH: usize = 40_000;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct WebFetchArgs {
    #[schemars(description = "The http/https URL to fetch.")]
    url: String,
    #[serde(default)]
    #[schemars(
        with = "u32",
        description = "Cap the returned characters. Ask for more only when the page was truncated and you need the rest."
    )]
    max_length: Option<u32>,
}

#[tool(
    name = "web_fetch",
    args = WebFetchArgs,
    execution = parallel,
    description = "Fetch an http/https URL and return its readable content as \
                   markdown, with navigation, ads and boilerplate stripped and \
                   links preserved. Use for reading a page you already have the \
                   URL for."
)]
pub struct WebFetchTool {
    http: WebClient,
    format: Format,
    max_length: usize,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new(WebClient::new())
    }
}

impl WebFetchTool {
    pub fn new(http: WebClient) -> Self {
        Self {
            http,
            format: Format::default(),
            max_length: DEFAULT_MAX_LENGTH,
        }
    }

    pub fn format(mut self, format: Format) -> Self {
        self.format = format;
        self
    }

    pub fn max_length(mut self, chars: usize) -> Self {
        self.max_length = chars.max(1);
        self
    }

    async fn tool(&self, args: WebFetchArgs, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let limit = args
            .max_length
            .map(|value| (value as usize).max(1))
            .unwrap_or(self.max_length);
        Ok(match self.render(&args.url).await {
            Ok(page) => ToolResult::ok(truncate(page, limit)),
            Err(error) => ToolResult::error(error),
        })
    }

    async fn render(&self, url: &str) -> Result<String, String> {
        let page = self.http.fetch(url).await?;
        let is_html = page.content_type.contains("html") || page.body.trim_start().starts_with('<');
        Ok(match (is_html, self.format) {
            (false, _) => page.body,
            (true, Format::Raw) => html_to_text(&page.body),
            (true, Format::Markdown) => to_markdown(&page.body, page.url.as_str()),
        })
    }
}

#[cfg(feature = "web-extract")]
fn to_markdown(body: &str, url: &str) -> String {
    match runic_extract::extract(body, Some(url)) {
        Ok(page) if !page.content.markdown.trim().is_empty() => page.content.markdown,
        _ => html_to_text(body),
    }
}

#[cfg(not(feature = "web-extract"))]
fn to_markdown(body: &str, _url: &str) -> String {
    html_to_text(body)
}

fn truncate(mut text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let cut = text
        .char_indices()
        .nth(limit)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.truncate(cut);
    text.push_str("\n\n… [truncated; call web_fetch again with a larger max_length for more]");
    text
}
