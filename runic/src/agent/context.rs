//! Conventional per-run config keys.
//!
//! Per-run context is now an open `serde_json::Map` carried on the request
//! (see `RunContext` / `AgentState::config`). The dev can put any keys they
//! want; these are the conventions Maia's hooks and factory understand. Use
//! the constants so the producer (`MaiaFactory::build_run_context`) and the
//! consumers (hooks) can't drift on spelling.

/// Tenant/user identity stamped onto `mcp__toolbox__*` calls by
/// [`super::hooks::BindToolContext`]. The thread is linked to `user_id`
/// when provided.
pub const KEY_USER_ID: &str = "user_id";
pub const KEY_ORG_ID: &str = "org_id";

/// Per-request web-search opt-in, gated by [`super::hooks::WebSearchGuard`].
pub const KEY_ALLOW_WEB_SEARCH: &str = "allow_web_search";

/// Per-request main-model override key, resolved against the provider
/// registry in `MaiaFactory::build_run_context`.
pub const KEY_PROVIDER: &str = "provider";
