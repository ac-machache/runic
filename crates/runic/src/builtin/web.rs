//! `web_fetch` + `web_search` — the agent's window onto the open web.
//!
//! `web_fetch` retrieves a URL and hands back readable text (HTML stripped to
//! plain text). It is guarded against **SSRF**: the IPv4/IPv6 non-global
//! classifiers are ported from zeroclaw's `domain_guard`, and every hop
//! (including redirects) is re-checked — host names are DNS-resolved and
//! rejected if *any* resolved address is private/loopback/reserved, which
//! closes the redirect- and DNS-rebind-bypass holes.
//!
//! `web_search` is provider-agnostic: it drives a [`SearchProvider`] injected
//! at construction (zeroclaw bakes a provider enum in; we keep the trait open
//! so the app picks Tavily / SearXNG / anything). Concrete [`TavilyProvider`]
//! and [`SearxngProvider`] ship here as ready defaults.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use runic_tool::{Tool, ToolContext, ToolResult};

const USER_AGENT: &str = "runic/0.1 (web_fetch)";
const MAX_REDIRECTS: usize = 5;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024; // 2 MiB of decoded text
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

// ───────────────────────────── SSRF guard ──────────────────────────────────

/// IPv4 addresses that must never be reachable from a fetch — loopback, the
/// RFC1918 private ranges, link-local, CGNAT, test-nets, and reserved space.
/// Ported from zeroclaw's `is_non_global_v4`.
fn is_non_global_v4(v4: Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || (a == 100 && (64..=127).contains(&b)) // 100.64.0.0/10 CGNAT
        || a >= 240 // 240.0.0.0/4 reserved
        || (a == 192 && b == 0 && (c == 0 || c == 2)) // 192.0.0.0/24, 192.0.2.0/24
        || (a == 198 && b == 51) // 198.51.100.0/24 TEST-NET-2
        || (a == 203 && b == 0) // 203.0.113.0/24 TEST-NET-3
        || (a == 198 && (18..=19).contains(&b)) // 198.18.0.0/15 benchmarking
}

/// IPv6 counterpart — loopback, unique-local, link-local, documentation, and
/// IPv4-mapped addresses that fall in a blocked v4 range.
fn is_non_global_v6(v6: Ipv6Addr) -> bool {
    let segs = v6.segments();
    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        || (segs[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
        || (segs[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        || (segs[0] == 0x2001 && segs[1] == 0x0db8) // 2001:db8::/32 documentation
        || v6.to_ipv4_mapped().is_some_and(is_non_global_v4)
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_non_global_v4(v4),
        IpAddr::V6(v6) => is_non_global_v6(v6),
    }
}

/// Host *names* that obviously point at the local machine or LAN, caught
/// before we even resolve them.
fn is_local_name(host: &str) -> bool {
    let h = host
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    h == "localhost" || h.ends_with(".localhost") || h.ends_with(".local")
}

/// Parse + SSRF-check a URL. IP literals are checked directly; host names are
/// resolved and rejected if *any* address is non-global.
async fn guard_url(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(format!("unsupported scheme '{other}' (only http/https)")),
    }
    let host = parsed.host_str().ok_or("url has no host")?.to_string();
    if is_local_name(&host) {
        return Err("refusing to fetch a local/loopback host".into());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err("refusing to fetch a private/reserved IP".into());
        }
    } else {
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addrs = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| format!("dns lookup failed: {e}"))?;
        let mut resolved = false;
        for a in addrs {
            resolved = true;
            if is_blocked_ip(a.ip()) {
                return Err("host resolves to a private/reserved IP".into());
            }
        }
        if !resolved {
            return Err("host did not resolve".into());
        }
    }
    Ok(parsed)
}

// ───────────────────────────── HTML → text ─────────────────────────────────

/// Drop `<tag …>…</tag>` sections (case-insensitive) wholesale — used to strip
/// `<script>`/`<style>` whose contents are not prose.
fn remove_blocks(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let (mut rest, mut lrest) = (html, lower.as_str());
    loop {
        let Some(start) = lrest.find(&open) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        match lrest[start..].find(&close) {
            Some(rel) => {
                let end = start + rel + close.len();
                rest = &rest[end..];
                lrest = &lrest[end..];
            }
            None => break, // unterminated → drop the rest
        }
    }
    out
}

/// Decode HTML entities (`&amp;`, `&#39;`, `&#x41;`, …). Pure; exposed for
/// fuzzing — must never panic on arbitrary input.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // Find the entity terminator, but only accept short entities (≤12
        // bytes). `find` returns a char-safe byte index — slicing `tail[..12]`
        // directly panics when byte 12 lands inside a multi-byte char.
        let Some(semi) = tail.find(';').filter(|&p| p <= 12) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| {
                    n.strip_prefix(['x', 'X'])
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .or_else(|| n.parse::<u32>().ok())
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Collapse trailing per-line whitespace and runs of >1 blank line.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0;
    for line in s.lines() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            blanks += 1;
            if blanks <= 1 {
                out.push('\n');
            }
        } else {
            blanks = 0;
            out.push_str(t.trim_start());
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// A small, dependency-free HTML→text pass: strip script/style, turn
/// block-level tags into line breaks, drop the rest of the markup, decode
/// entities. Good enough for a model to read; not a full renderer. Pure;
/// exposed for fuzzing — must never panic on arbitrary input.
pub fn html_to_text(html: &str) -> String {
    let cleaned = remove_blocks(&remove_blocks(html, "script"), "style");
    let mut out = String::with_capacity(cleaned.len());
    let mut tag = String::new();
    let mut in_tag = false;
    for c in cleaned.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let name = tag
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if matches!(
                    name.as_str(),
                    "p" | "br"
                        | "div"
                        | "li"
                        | "tr"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "section"
                        | "article"
                        | "header"
                        | "footer"
                        | "ul"
                        | "ol"
                        | "table"
                        | "blockquote"
                        | "pre"
                        | "hr"
                ) {
                    out.push('\n');
                }
            }
            _ if in_tag => tag.push(c),
            _ => out.push(c),
        }
    }
    collapse_ws(&decode_entities(&out))
}

// ───────────────────────────── web_fetch ───────────────────────────────────

/// Fetch a URL and return readable text. SSRF-guarded.
pub struct WebFetchTool {
    client: reqwest::Client,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            // Follow redirects manually so each hop is re-guarded.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT)
            .build()
            .expect("reqwest client builds with static config");
        Self { client }
    }

    async fn fetch(&self, url: &str) -> Result<String, String> {
        let mut current = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let target = guard_url(&current).await?;
            let resp = self
                .client
                .get(target.clone())
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?;
            let status = resp.status();

            if status.is_redirection() {
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or("redirect without a Location header")?;
                // Resolve relative redirects against the current URL.
                current = target
                    .join(loc)
                    .map_err(|e| format!("bad redirect target: {e}"))?
                    .to_string();
                continue;
            }
            if !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }

            let ctype = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            let is_textual = ctype.is_empty()
                || ctype.contains("html")
                || ctype.contains("text/")
                || ctype.contains("json")
                || ctype.contains("xml");
            if !is_textual {
                return Err(format!("unsupported content-type '{ctype}'"));
            }

            let bytes = resp
                .bytes()
                .await
                .map_err(|e| format!("read failed: {e}"))?;
            let body = String::from_utf8_lossy(&bytes);
            let text = if ctype.contains("html") || body.trim_start().starts_with('<') {
                html_to_text(&body)
            } else {
                body.into_owned()
            };
            let mut text = text;
            if text.len() > MAX_BODY_BYTES {
                // Truncate on a char boundary.
                let mut cut = MAX_BODY_BYTES;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                text.push_str("\n\n… [truncated]");
            }
            return Ok(text);
        }
        Err("too many redirects".into())
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch an http/https URL and return its readable text content (HTML is \
         stripped to plain text). Use for reading a specific page you already \
         have the URL for."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The http/https URL to fetch." }
            },
            "required": ["url"]
        })
    }
    fn parallelizable(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("web_fetch requires `url`"));
        };
        match self.fetch(url).await {
            Ok(text) => Ok(ToolResult::ok(text)),
            Err(e) => Ok(ToolResult::error(e)),
        }
    }
}

// ───────────────────────────── web_search ──────────────────────────────────

/// One search hit, normalized across providers.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// A pluggable backend for `web_search`. The app picks (and credentials) the
/// provider; the tool just drives this trait.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// Display name (shown to the model in the result header).
    fn name(&self) -> &str;
    /// Run a query, returning up to `max_results` hits.
    async fn search(&self, query: &str, max_results: usize) -> anyhow::Result<Vec<SearchResult>>;
}

/// Search the web via an injected [`SearchProvider`].
pub struct WebSearchTool {
    provider: Arc<dyn SearchProvider>,
    max_results: usize,
}

impl WebSearchTool {
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        Self {
            provider,
            max_results: 5,
        }
    }
    pub fn with_max_results(mut self, n: usize) -> Self {
        self.max_results = n.clamp(1, 10);
        self
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web and return a ranked list of results (title, URL, \
         snippet). Use to discover pages; follow up with web_fetch to read one."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query. Be specific." }
            },
            "required": ["query"]
        })
    }
    fn parallelizable(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("web_search requires `query`"));
        };
        let results = match self.provider.search(query, self.max_results).await {
            Ok(r) => r,
            Err(e) => return Ok(ToolResult::error(format!("search failed: {e}"))),
        };
        if results.is_empty() {
            return Ok(ToolResult::ok(format!("No results for \"{query}\".")));
        }
        let mut out = format!(
            "Search results for \"{query}\" (via {}):\n",
            self.provider.name()
        );
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!(
                "\n{}. {}\n   {}\n   {}\n",
                i + 1,
                r.title,
                r.url,
                r.snippet
            ));
        }
        Ok(ToolResult::ok(out))
    }
}

// ── Tavily (POST JSON, bearer key) ──────────────────────────────────────────

/// [`SearchProvider`] backed by Tavily (`api.tavily.com`), an LLM-native search
/// API. Requires an API key.
pub struct TavilyProvider {
    api_key: String,
    client: reqwest::Client,
}

impl TavilyProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            client: reqwest::Client::new(),
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
    async fn search(&self, query: &str, max_results: usize) -> anyhow::Result<Vec<SearchResult>> {
        let resp = self
            .client
            .post("https://api.tavily.com/search")
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
            .map(|h| SearchResult {
                title: h.title,
                url: h.url,
                snippet: h.content,
            })
            .collect())
    }
}

// ── SearXNG (GET JSON, self-hosted, no key) ─────────────────────────────────

/// [`SearchProvider`] backed by a self-hosted SearXNG instance (no API key).
pub struct SearxngProvider {
    base_url: String,
    client: reqwest::Client,
}

impl SearxngProvider {
    /// `base_url` is the instance root, e.g. `https://searx.example.com`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
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
    async fn search(&self, query: &str, max_results: usize) -> anyhow::Result<Vec<SearchResult>> {
        let resp = self
            .client
            .get(format!("{}/search", self.base_url))
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
            .map(|h| SearchResult {
                title: h.title,
                url: h.url,
                snippet: h.content,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_blocks_private_and_reserved() {
        assert!(is_blocked_ip("127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("10.1.2.3".parse().unwrap()));
        assert!(is_blocked_ip("192.168.0.1".parse().unwrap()));
        assert!(is_blocked_ip("169.254.169.254".parse().unwrap())); // cloud metadata
        assert!(is_blocked_ip("100.64.0.1".parse().unwrap())); // CGNAT
        assert!(is_blocked_ip("::1".parse().unwrap()));
        assert!(is_blocked_ip("fc00::1".parse().unwrap()));
        // public addresses pass
        assert!(!is_blocked_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_blocked_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_blocked_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[tokio::test]
    async fn guard_rejects_local_names_and_ip_literals() {
        assert!(guard_url("http://localhost/x").await.is_err());
        assert!(guard_url("http://foo.local/").await.is_err());
        assert!(guard_url("http://127.0.0.1:8080/").await.is_err());
        assert!(
            guard_url("http://169.254.169.254/latest/meta-data")
                .await
                .is_err()
        );
        assert!(guard_url("ftp://example.com/").await.is_err()); // scheme
        assert!(guard_url("not a url").await.is_err());
    }

    #[test]
    fn html_to_text_strips_markup_and_decodes() {
        let html = "<html><head><style>.x{color:red}</style></head>\
            <body><h1>Title</h1><script>alert(1)</script>\
            <p>Hello &amp; welcome to <b>runic</b>.</p>\
            <ul><li>one</li><li>two</li></ul></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & welcome to runic."));
        assert!(text.contains("one"));
        assert!(text.contains("two"));
        // script/style contents gone
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
    }

    #[test]
    fn entity_decoding_handles_numeric() {
        assert_eq!(decode_entities("a&#65;b"), "aAb");
        assert_eq!(decode_entities("&#x41;"), "A");
        assert_eq!(
            decode_entities("plain &unknown; text"),
            "plain &unknown; text"
        );
    }

    #[test]
    fn entity_decoding_is_char_boundary_safe() {
        // Regression: byte 12 after the `&` lands inside the multi-byte 'à'.
        // The old `tail[..12]` slice panicked here.
        assert_eq!(
            decode_entities("&#039;aide à la décision"),
            "'aide à la décision"
        );
        // A bare `&` followed by multibyte text and no nearby ';' must not panic.
        assert_eq!(
            decode_entities("R&D coûte à la société"),
            "R&D coûte à la société"
        );
    }
}
