use std::sync::Arc;

use async_trait::async_trait;
use runic::Llm;
use runic::builtin::{Format, SearchProvider, SearchResult, Web, WebClient, WebFetchTool};
use runic::composer::{Agent, Composer, Runtime};
use runic::tool::{Tool, ToolContext};

struct Nowhere;

#[async_trait]
impl SearchProvider for Nowhere {
    fn name(&self) -> &str {
        "nowhere"
    }
    async fn search(
        &self,
        _http: &WebClient,
        _query: &str,
        _max: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        Ok(vec![])
    }
}

struct Stub;

#[async_trait]
impl runic_provider::Provider for Stub {
    async fn complete(
        &self,
        _req: runic_provider::CompletionRequest,
    ) -> Result<runic_provider::CompletionResponse, runic_provider::ProviderError> {
        Err(runic_provider::ProviderError::Parse("unused".into()))
    }
}

async fn tool_names(web: Web) -> Vec<String> {
    let agent = Agent::new(Llm::new(Arc::new(Stub), "test-model")).with(web);
    let views = Composer::new(agent, Runtime::new())
        .describe("alice", "s1")
        .await
        .unwrap();
    views
        .iter()
        .flat_map(|view| view.tools.iter().map(|spec| spec.name.clone()))
        .collect()
}

#[tokio::test]
async fn search_is_opt_in() {
    assert_eq!(tool_names(Web::new()).await, vec!["web_fetch".to_string()]);

    let with_search = tool_names(Web::new().search(Nowhere)).await;
    assert_eq!(with_search, vec!["web_fetch", "web_search"]);
}

#[test]
fn the_fetch_schema_offers_an_optional_max_length() {
    let schema = WebFetchTool::new(WebClient::new()).parameters_schema();

    assert_eq!(schema["properties"]["url"]["type"], "string");
    assert_eq!(schema["properties"]["max_length"]["type"], "integer");

    let required = schema["required"].as_array().unwrap();
    assert!(required.iter().any(|name| name == "url"));
    assert!(
        !required.iter().any(|name| name == "max_length"),
        "max_length must stay optional: {schema}"
    );
}

#[tokio::test]
async fn a_blocked_host_comes_back_as_an_in_band_error() {
    let ctx = ToolContext::new("alice", "s1", "r1");
    let result = WebFetchTool::new(WebClient::new())
        .execute(serde_json::json!({ "url": "http://[::1]/admin" }), &ctx)
        .await
        .unwrap();

    assert!(result.is_error());
    assert!(
        result.text().contains("private/reserved"),
        "unexpected error: {}",
        result.text()
    );
}

#[cfg(feature = "web-extract")]
#[test]
fn markdown_mode_strips_chrome_and_keeps_links() {
    let html = r#"<html><body>
        <nav><a href="/elsewhere">Nav Link</a></nav>
        <article><h1>The Headline</h1>
        <p>Body text with a <a href="/deep">real link</a> inside it.</p></article>
        <footer>Copyright notice</footer>
    </body></html>"#;

    let page = runic_extract::extract(html, Some("https://example.com/post")).unwrap();
    let md = page.content.markdown;

    assert!(md.contains("# The Headline"), "{md}");
    assert!(md.contains("https://example.com/deep"), "{md}");
    assert!(!md.contains("Nav Link"), "{md}");
    assert!(!md.contains("Copyright notice"), "{md}");
}

#[test]
fn raw_mode_is_available_without_the_extract_feature() {
    let tool = WebFetchTool::new(WebClient::new())
        .format(Format::Raw)
        .max_length(10);
    assert_eq!(tool.name(), "web_fetch");
}
