//! `composio` — one meta-tool over [Composio](https://composio.dev)'s v3 API,
//! giving the agent ~1000 external app actions (Gmail, Slack, GitHub, Notion…)
//! behind a single tool.
//!
//! Ported (leaner) from zeroclaw's `composio.rs`. Like the original it is a
//! **single dispatcher tool**, not dynamic per-action registration: the model
//! calls `composio` with an `action` of `list` / `execute` / `list_accounts` /
//! `connect`. The hardened ergonomics that make it actually work are kept:
//!
//! - **slug normalization + candidates** — the model rarely knows Composio's
//!   exact slug, so we try sensible variants (`gmail_send_email`,
//!   `GMAIL_SEND_EMAIL`, `gmail-send-email`) and only advance past a 404;
//! - **auto connected-account resolution** — pick the first usable OAuth
//!   account for the `(entity, app)` pair and cache it;
//! - **NLP fallback** — if the model has no structured `params`, it can pass
//!   free-text `text` and let Composio infer the arguments.
//!
//! Trimmed vs. zeroclaw: no SecurityPolicy gate (runic gates via hooks), no
//! proxy client, no encrypted-secret plumbing, no error-schema round-trip.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use runic_tool::{Tool, ToolContext, ToolResult};

const API_BASE: &str = "https://backend.composio.dev/api/v3";
const TOOL_VERSION: &str = "latest";

/// One meta-tool fronting the whole Composio catalog for a single API key.
pub struct ComposioTool {
    api_key: String,
    default_entity_id: String,
    client: reqwest::Client,
    /// `action_name` → resolved Composio slug.
    slug_cache: RwLock<HashMap<String, String>>,
    /// `entity:app` → connected_account_id.
    account_cache: RwLock<HashMap<String, String>>,
}

impl ComposioTool {
    /// `entity_id` scopes per-user OAuth connections; defaults to `"default"`.
    pub fn new(api_key: impl Into<String>, entity_id: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client builds with static config");
        Self {
            api_key: api_key.into(),
            default_entity_id: entity_id.unwrap_or_else(|| "default".to_string()),
            client,
            slug_cache: RwLock::new(HashMap::new()),
            account_cache: RwLock::new(HashMap::new()),
        }
    }
}

// ───────────────────────── pure helpers (unit-tested) ───────────────────────

/// Lowercase, trim, and hyphenate an app/toolkit slug.
fn normalize_app_slug(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace('_', "-")
}

/// Composio scopes OAuth by entity (user/workspace); normalize loosely.
fn normalize_entity_id(s: &str) -> String {
    s.trim().to_string()
}

/// Guess the app from an action name's prefix: `gmail-send-email` → `gmail`.
fn infer_app_from_action(action: &str) -> Option<String> {
    let cut = action.find(['-', '_'])?;
    let app = &action[..cut];
    (!app.is_empty()).then(|| normalize_app_slug(app))
}

/// Candidate slugs to try, most-likely first, de-duplicated in order.
fn slug_candidates(action: &str) -> Vec<String> {
    let t = action.trim();
    let mut out = Vec::new();
    for c in [
        t.to_string(),
        t.to_ascii_lowercase(),
        t.to_ascii_uppercase(),
        t.replace('-', "_"),
        t.replace('_', "-"),
        t.to_ascii_uppercase().replace('-', "_"),
        t.to_ascii_lowercase().replace('_', "-"),
    ] {
        if !c.is_empty() && !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// Composio account statuses we can actually call through.
fn is_usable_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("ACTIVE")
        || status.eq_ignore_ascii_case("INITIALIZING")
        || status.eq_ignore_ascii_case("INITIATED")
}

/// Build the v3 execute body. Structured `params` and free-text `text` are
/// mutually exclusive (text triggers Composio's NLP arg inference).
fn build_execute_body(
    params: &serde_json::Value,
    text: Option<&str>,
    entity_id: &str,
    account_id: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::Map::new();
    body.insert("version".into(), TOOL_VERSION.into());
    match text {
        Some(t) if !t.trim().is_empty() => {
            body.insert("text".into(), t.into());
        }
        _ => {
            body.insert("arguments".into(), params.clone());
        }
    }
    body.insert("user_id".into(), entity_id.into());
    if let Some(acc) = account_id {
        body.insert("connected_account_id".into(), acc.into());
    }
    serde_json::Value::Object(body)
}

/// Recursively find the first string value under `key` anywhere in `v`.
fn find_string<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(s) = map.get(key).and_then(|x| x.as_str()) {
                return Some(s);
            }
            map.values().find_map(|child| find_string(child, key))
        }
        serde_json::Value::Array(arr) => arr.iter().find_map(|child| find_string(child, key)),
        _ => None,
    }
}

// ───────────────────────────── API response DTOs ───────────────────────────

#[derive(Deserialize)]
struct ToolsResponse {
    #[serde(default)]
    items: Vec<V3Tool>,
}
#[derive(Deserialize)]
struct V3Tool {
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct AccountsResponse {
    #[serde(default)]
    items: Vec<ConnectedAccount>,
}
#[derive(Deserialize)]
struct ConnectedAccount {
    id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    toolkit: Option<ToolkitRef>,
}
#[derive(Deserialize)]
struct ToolkitRef {
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Deserialize)]
struct AuthConfigsResponse {
    #[serde(default)]
    items: Vec<AuthConfig>,
}
#[derive(Deserialize)]
struct AuthConfig {
    id: String,
}

// ───────────────────────────── HTTP operations ─────────────────────────────

impl ComposioTool {
    fn entity<'a>(&'a self, arg: Option<&'a str>) -> String {
        normalize_entity_id(arg.unwrap_or(&self.default_entity_id))
    }

    async fn list_actions(&self, app: Option<&str>) -> anyhow::Result<Vec<V3Tool>> {
        let mut req = self
            .client
            .get(format!("{API_BASE}/tools"))
            .header("x-api-key", &self.api_key)
            .query(&[("limit", "100"), ("toolkit_versions", TOOL_VERSION)]);
        if let Some(app) = app {
            let slug = normalize_app_slug(app);
            req = req.query(&[("toolkits", slug.as_str())]);
        }
        let resp = req.send().await?.error_for_status()?.json::<ToolsResponse>().await?;
        // Warm the slug cache so a later execute can skip candidate-guessing.
        if let Ok(mut cache) = self.slug_cache.write() {
            for t in &resp.items {
                if let (Some(name), Some(slug)) = (&t.name, &t.slug) {
                    cache.insert(name.to_ascii_lowercase(), slug.clone());
                }
            }
        }
        Ok(resp.items)
    }

    async fn list_accounts(&self, app: Option<&str>, entity: &str) -> anyhow::Result<Vec<ConnectedAccount>> {
        let mut q: Vec<(&str, String)> = vec![
            ("limit", "50".into()),
            ("order_by", "updated_at".into()),
            ("order_direction", "desc".into()),
            ("user_ids", entity.to_string()),
        ];
        if let Some(app) = app {
            q.push(("toolkit_slugs", normalize_app_slug(app)));
        }
        let resp = self
            .client
            .get(format!("{API_BASE}/connected_accounts"))
            .header("x-api-key", &self.api_key)
            .query(&q)
            .send()
            .await?
            .error_for_status()?
            .json::<AccountsResponse>()
            .await?;
        Ok(resp.items)
    }

    /// Resolve a usable connected-account id for `(entity, app)`, cache-first.
    async fn resolve_account(&self, app: Option<&str>, entity: &str) -> Option<String> {
        let app = app.map(normalize_app_slug);
        let cache_key = format!("{entity}:{}", app.as_deref().unwrap_or("*"));
        if let Ok(cache) = self.account_cache.read()
            && let Some(id) = cache.get(&cache_key) {
                return Some(id.clone());
            }
        let accounts = self.list_accounts(app.as_deref(), entity).await.ok()?;
        let id = accounts
            .into_iter()
            .find(|a| {
                is_usable_status(&a.status)
                    && app
                        .as_deref()
                        .is_none_or(|want| a.toolkit.as_ref().and_then(|t| t.slug.as_deref()) == Some(want))
            })
            .map(|a| a.id)?;
        if let Ok(mut cache) = self.account_cache.write() {
            cache.insert(cache_key, id.clone());
        }
        Some(id)
    }

    /// POST one execute attempt. `Ok(None)` means 404 (try the next slug);
    /// `Ok(Some)` is the result; `Err` is a real failure.
    async fn try_execute(&self, slug: &str, body: &serde_json::Value) -> anyhow::Result<Option<serde_json::Value>> {
        let resp = self
            .client
            .post(format!("{API_BASE}/tools/execute/{slug}"))
            .header("x-api-key", &self.api_key)
            .json(body)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let code = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Composio execute failed ({code}): {}", text.chars().take(400).collect::<String>());
        }
        Ok(Some(resp.json::<serde_json::Value>().await?))
    }

    async fn execute_action(
        &self,
        action: &str,
        app: Option<&str>,
        params: &serde_json::Value,
        text: Option<&str>,
        entity: &str,
        account_arg: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let app = app.map(|a| a.to_string()).or_else(|| infer_app_from_action(action));
        let account = match account_arg {
            Some(a) => Some(a.to_string()),
            None => self.resolve_account(app.as_deref(), entity).await,
        };
        let body = build_execute_body(params, text, entity, account.as_deref());

        // Cached slug first, then normalized candidates.
        let mut candidates = Vec::new();
        if let Ok(cache) = self.slug_cache.read()
            && let Some(slug) = cache.get(&action.to_ascii_lowercase()) {
                candidates.push(slug.clone());
            }
        for c in slug_candidates(action) {
            if !candidates.contains(&c) {
                candidates.push(c);
            }
        }

        for slug in &candidates {
            match self.try_execute(slug, &body).await? {
                Some(result) => return Ok(result),
                None => continue, // 404 → next candidate
            }
        }
        anyhow::bail!(
            "no Composio action matched '{action}' (tried: {}). \
             Run action=list first to find the exact slug.",
            candidates.join(", ")
        )
    }

    async fn connect(&self, app: &str, entity: &str, auth_config_arg: Option<&str>) -> anyhow::Result<String> {
        let app = normalize_app_slug(app);
        let auth_config_id = match auth_config_arg {
            Some(id) => id.to_string(),
            None => {
                let resp = self
                    .client
                    .get(format!("{API_BASE}/auth_configs"))
                    .header("x-api-key", &self.api_key)
                    .query(&[("toolkit_slug", app.as_str())])
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<AuthConfigsResponse>()
                    .await?;
                resp.items
                    .into_iter()
                    .next()
                    .map(|c| c.id)
                    .ok_or_else(|| anyhow::anyhow!("no auth config found for '{app}'; pass auth_config_id"))?
            }
        };
        let resp = self
            .client
            .post(format!("{API_BASE}/connected_accounts/link"))
            .header("x-api-key", &self.api_key)
            .json(&serde_json::json!({ "auth_config_id": auth_config_id, "user_id": entity }))
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        find_string(&resp, "redirect_url")
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("Composio did not return a redirect_url"))
    }
}

// ───────────────────────────── Tool impl ───────────────────────────────────

#[async_trait]
impl Tool for ComposioTool {
    fn name(&self) -> &str {
        "composio"
    }
    fn description(&self) -> &str {
        "Run actions on 1000+ external apps (Gmail, Slack, GitHub, Notion, …) \
         via Composio. Set `action`: `list` to find action slugs for an `app`; \
         `execute` to run `action_name` with `params` (or free-text `text`); \
         `list_accounts` to see connected OAuth accounts; `connect` to start an \
         OAuth link for an `app`."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "execute", "list_accounts", "connect"],
                    "description": "What to do."
                },
                "app": { "type": "string", "description": "App/toolkit slug, e.g. 'gmail'." },
                "action_name": { "type": "string", "description": "Action slug to execute, e.g. 'gmail-send-email'." },
                "params": { "type": "object", "description": "Structured arguments for the action." },
                "text": { "type": "string", "description": "Natural-language request; Composio infers args (use instead of params)." },
                "entity_id": { "type": "string", "description": "User/workspace id scoping OAuth (defaults to the configured entity)." },
                "connected_account_id": { "type": "string", "description": "Explicit connected account to use." },
                "auth_config_id": { "type": "string", "description": "Auth config for `connect` (auto-resolved if omitted)." }
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        let Some(action) = args.get("action").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("composio requires `action`"));
        };
        let app = args.get("app").and_then(|v| v.as_str());
        let entity = self.entity(args.get("entity_id").and_then(|v| v.as_str()));

        match action {
            "list" => match self.list_actions(app).await {
                Ok(tools) => {
                    if tools.is_empty() {
                        return Ok(ToolResult::ok("No actions found."));
                    }
                    let mut out = format!("Composio actions{}:\n", app.map(|a| format!(" for {a}")).unwrap_or_default());
                    for t in tools.iter().take(60) {
                        let slug = t.slug.as_deref().or(t.name.as_deref()).unwrap_or("?");
                        let desc = t.description.as_deref().unwrap_or("");
                        out.push_str(&format!("- {slug} — {}\n", desc.chars().take(100).collect::<String>()));
                    }
                    Ok(ToolResult::ok(out))
                }
                Err(e) => Ok(ToolResult::error(format!("composio list failed: {e}"))),
            },
            "list_accounts" | "connected_accounts" => match self.list_accounts(app, &entity).await {
                Ok(accounts) => {
                    if accounts.is_empty() {
                        return Ok(ToolResult::ok("No connected accounts."));
                    }
                    let mut out = String::from("Connected accounts:\n");
                    for a in &accounts {
                        let tk = a.toolkit.as_ref().and_then(|t| t.slug.as_deref()).unwrap_or("?");
                        out.push_str(&format!("- {tk}: {} ({})\n", a.id, a.status));
                    }
                    Ok(ToolResult::ok(out))
                }
                Err(e) => Ok(ToolResult::error(format!("composio list_accounts failed: {e}"))),
            },
            "execute" => {
                let Some(action_name) = args
                    .get("action_name")
                    .or_else(|| args.get("tool_slug"))
                    .and_then(|v| v.as_str())
                else {
                    return Ok(ToolResult::error("composio execute requires `action_name`"));
                };
                let params = args.get("params").cloned().unwrap_or_else(|| serde_json::json!({}));
                let text = args.get("text").and_then(|v| v.as_str());
                let account = args.get("connected_account_id").and_then(|v| v.as_str());
                match self.execute_action(action_name, app, &params, text, &entity, account).await {
                    Ok(result) => Ok(ToolResult::ok(
                        serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()),
                    )),
                    Err(e) => Ok(ToolResult::error(format!("{e}"))),
                }
            }
            "connect" => {
                let Some(app) = app else {
                    return Ok(ToolResult::error("composio connect requires `app`"));
                };
                let auth = args.get("auth_config_id").and_then(|v| v.as_str());
                match self.connect(app, &entity, auth).await {
                    Ok(url) => Ok(ToolResult::ok(format!(
                        "To connect {app}, open this URL and authorize:\n{url}"
                    ))),
                    Err(e) => Ok(ToolResult::error(format!("composio connect failed: {e}"))),
                }
            }
            other => Ok(ToolResult::error(format!("unknown composio action '{other}'"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_inference_and_normalization() {
        assert_eq!(infer_app_from_action("gmail-send-email").as_deref(), Some("gmail"));
        assert_eq!(infer_app_from_action("GITHUB_CREATE_ISSUE").as_deref(), Some("github"));
        assert_eq!(infer_app_from_action("noprefix"), None);
        assert_eq!(normalize_app_slug("  Gmail_API "), "gmail-api");
    }

    #[test]
    fn slug_candidates_cover_common_casings_without_dupes() {
        let c = slug_candidates("gmail-send-email");
        assert!(c.contains(&"gmail-send-email".to_string()));
        assert!(c.contains(&"gmail_send_email".to_string()));
        assert!(c.contains(&"GMAIL-SEND-EMAIL".to_string()));
        assert!(c.contains(&"GMAIL_SEND_EMAIL".to_string()));
        // no duplicates
        let mut sorted = c.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), c.len());
    }

    #[test]
    fn usable_status_matches_composio_states() {
        assert!(is_usable_status("ACTIVE"));
        assert!(is_usable_status("initializing"));
        assert!(is_usable_status("INITIATED"));
        assert!(!is_usable_status("DISCONNECTED"));
        assert!(!is_usable_status(""));
    }

    #[test]
    fn execute_body_picks_text_xor_arguments() {
        let params = serde_json::json!({ "to": "a@b.com" });
        let with_args = build_execute_body(&params, None, "default", Some("acc_1"));
        assert_eq!(with_args["arguments"]["to"], "a@b.com");
        assert!(with_args.get("text").is_none());
        assert_eq!(with_args["connected_account_id"], "acc_1");
        assert_eq!(with_args["user_id"], "default");

        let with_text = build_execute_body(&params, Some("email bob hello"), "u1", None);
        assert_eq!(with_text["text"], "email bob hello");
        assert!(with_text.get("arguments").is_none());
        assert!(with_text.get("connected_account_id").is_none());
    }

    #[test]
    fn find_string_digs_into_nested_json() {
        let v = serde_json::json!({
            "data": { "connection": { "redirect_url": "https://auth.example/x" } }
        });
        assert_eq!(find_string(&v, "redirect_url"), Some("https://auth.example/x"));
        assert_eq!(find_string(&v, "missing"), None);
    }
}
