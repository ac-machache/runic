use std::sync::Arc;

use async_trait::async_trait;
use runic_macros::tool;
use runic_tool::{ToolContext, ToolResult};
use serde::Deserialize;

use super::client::WebClient;

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Backends run their request through the caller's [`WebClient`] so the SSRF
/// guard and body cap apply to search endpoints too.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &str;

    async fn search(
        &self,
        http: &WebClient,
        query: &str,
        max_results: usize,
    ) -> anyhow::Result<Vec<SearchResult>>;
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct WebSearchArgs {
    #[schemars(description = "The search query. Be specific.")]
    query: String,
}

#[tool(
    name = "web_search",
    args = WebSearchArgs,
    execution = parallel,
    description = "Search the web and return a ranked list of results (title, \
                   URL, snippet). Use to discover pages; follow up with \
                   web_fetch to read one."
)]
pub struct WebSearchTool {
    http: WebClient,
    provider: Arc<dyn SearchProvider>,
    max_results: usize,
}

impl WebSearchTool {
    pub fn new(http: WebClient, provider: Arc<dyn SearchProvider>) -> Self {
        Self {
            http,
            provider,
            max_results: 5,
        }
    }

    pub fn with_max_results(mut self, results: usize) -> Self {
        self.max_results = results.clamp(1, 10);
        self
    }

    async fn tool(&self, args: WebSearchArgs, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let query = args.query.as_str();
        let results = match self
            .provider
            .search(&self.http, query, self.max_results)
            .await
        {
            Ok(results) => results,
            Err(error) => return Ok(ToolResult::error(format!("search failed: {error}"))),
        };
        if results.is_empty() {
            return Ok(ToolResult::ok(format!("No results for \"{query}\".")));
        }
        let mut out = format!(
            "Search results for \"{query}\" (via {}):\n",
            self.provider.name()
        );
        for (rank, hit) in results.iter().enumerate() {
            out.push_str(&format!(
                "\n{}. {}\n   {}\n   {}\n",
                rank + 1,
                hit.title,
                hit.url,
                hit.snippet
            ));
        }
        Ok(ToolResult::ok(out))
    }
}

const TAVILY_URL: &str = "https://api.tavily.com/search";

pub struct TavilyProvider {
    api_key: String,
}

impl TavilyProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
        }
    }
}

#[derive(Deserialize)]
struct TavilyResp {
    #[serde(default)]
    results: Vec<TavilyHit>,
}

#[derive(Deserialize)]
struct TavilyHit {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

#[async_trait]
impl SearchProvider for TavilyProvider {
    fn name(&self) -> &str {
        "Tavily"
    }

    async fn search(
        &self,
        http: &WebClient,
        query: &str,
        max_results: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let target = http.guard(TAVILY_URL).await.map_err(anyhow::Error::msg)?;
        let resp = http
            .http()
            .post(target)
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({
                "query": query,
                "max_results": max_results,
                "search_depth": "basic",
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<TavilyResp>()
            .await?;
        Ok(resp
            .results
            .into_iter()
            .map(|hit| SearchResult {
                title: hit.title,
                url: hit.url,
                snippet: hit.content,
            })
            .collect())
    }
}

pub struct SearxngProvider {
    base_url: String,
}

impl SearxngProvider {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }
}

#[derive(Deserialize)]
struct SearxResp {
    #[serde(default)]
    results: Vec<SearxHit>,
}

#[derive(Deserialize)]
struct SearxHit {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

#[async_trait]
impl SearchProvider for SearxngProvider {
    fn name(&self) -> &str {
        "SearXNG"
    }

    async fn search(
        &self,
        http: &WebClient,
        query: &str,
        max_results: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let target = http
            .guard(&format!("{}/search", self.base_url))
            .await
            .map_err(anyhow::Error::msg)?;
        let resp = http
            .http()
            .get(target)
            .query(&[("q", query), ("format", "json")])
            .send()
            .await?
            .error_for_status()?
            .json::<SearxResp>()
            .await?;
        Ok(resp
            .results
            .into_iter()
            .take(max_results)
            .map(|hit| SearchResult {
                title: hit.title,
                url: hit.url,
                snippet: hit.content,
            })
            .collect())
    }
}
